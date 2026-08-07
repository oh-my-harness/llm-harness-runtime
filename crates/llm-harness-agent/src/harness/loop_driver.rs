// loop_driver.rs — loop driving methods extracted from harness/mod.rs
use std::sync::Arc;

use futures::StreamExt;
use llm_harness_loop::CompositePrepareNextTurnHook;
use llm_harness_loop::{
    CompositeAfterProviderResponseHook, CompositeAfterToolCallHook, CompositeAfterTurnHook,
    CompositeBeforeProviderRequestHook, CompositeBeforeRunHook, CompositeBeforeToolCallHook,
    CompositeBeforeTurnHook, CompositeFinalAnswerValidator, CompositeShouldStopHook,
    CompositeTransformContextHook, HookedTool, LoopConfig, agent_loop,
};
use llm_harness_types::*;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::session::types::SessionEntryPayload;

use super::AgentHarness;
use super::event::{AgentHarnessEvent, HarnessToolCallResult};
use super::prepare_next_turn::DefaultPrepareNextTurn;

impl AgentHarness {
    fn push_pending_write(&self, payload: SessionEntryPayload) {
        self.inner
            .lock()
            .unwrap()
            .state
            .pending_session_writes
            .push_back(payload);
    }

    async fn flush_pending_writes(&self) -> Result<usize, HarnessError> {
        let mut count = 0;
        loop {
            let payload = {
                let inner = self.inner.lock().unwrap();
                inner.state.pending_session_writes.front().cloned()
            };
            let Some(payload) = payload else { break };
            // Write before removing. If this fails, payload remains in pending.
            // The run_loop cleanup in the Err branch will clear the remainder.
            self.session.append(payload).await?;
            self.inner
                .lock()
                .unwrap()
                .state
                .pending_session_writes
                .pop_front();
            count += 1;
        }
        Ok(count)
    }

    pub(super) async fn run_before_run_hook(
        &self,
        run: &RunContext,
        mut initial: Vec<AgentMessage>,
    ) -> Result<Vec<AgentMessage>, HarnessError> {
        let (has_skill_readiness, skills, templates) = {
            let inner = self.inner.lock().unwrap();
            (
                inner.readable_skills.is_some(),
                inner.skills.clone(),
                inner.templates.clone(),
            )
        };
        // `readable_skills` 为 Some 表示 skill readiness 已激活（Builder 注册
        // 或运行时懒注册）。此条件下内联注入 skills 元数据，不依赖独立的
        // BeforeRunHook 注册——`readable_skills` 的存在本身就是激活信号。
        if !has_skill_readiness && self.hooks.before_run.is_empty() {
            return Ok(initial);
        }
        let resources = AgentHarnessResources { skills, templates };
        let prompt_text = initial
            .iter()
            .find_map(|m| {
                if let AgentMessage::User(u) = m {
                    u.content.iter().find_map(|c| {
                        if let ContentBlock::Text { text } = c {
                            Some(text.clone())
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
            })
            .unwrap_or_default();

        // 内联注入 skills 元数据（方式三第一层）。
        if has_skill_readiness {
            let metadata = crate::skills::format_skills_metadata(&resources.skills, true);
            if !metadata.is_empty() {
                initial.push(AgentMessage::User(UserMessage {
                    content: vec![ContentBlock::Text { text: metadata }],
                    timestamp: chrono::Utc::now(),
                }));
            }
        }

        if !self.hooks.before_run.is_empty() {
            let composite = Arc::new(CompositeBeforeRunHook::new(self.hooks.before_run.clone()));

            let mut system_prompt = self.inner.lock().unwrap().state.system_prompt.clone();
            let result = composite
                .before_run(BeforeRunCtx {
                    run,
                    prompt_text: &prompt_text,
                    initial_messages: &mut initial,
                    system_prompt: &mut system_prompt,
                    resources: &resources,
                })
                .await
                .map_err(HarnessError::from)?;

            // Apply hook results.
            initial.extend(result.additional_messages);
            if let Some(sp) = result.system_prompt.or(system_prompt) {
                self.inner.lock().unwrap().state.system_prompt = Some(sp);
            }
        }
        Ok(initial)
    }

    fn build_loop_config(
        &self,
        run: Arc<RunContext>,
        steer_rx: mpsc::Receiver<AgentMessage>,
        follow_up_rx: mpsc::Receiver<AgentMessage>,
        abort: CancellationToken,
    ) -> LoopConfig {
        // ── Snapshot state (lock is held via `inner` until `drop(inner)`) ──
        let (
            tools,
            retry,
            model,
            thinking_level,
            temperature,
            response_format,
            active_tools,
            final_answer_mode,
            max_tokens,
        ) = {
            let inner = self.inner.lock().unwrap();
            let st = &inner.state;
            (
                st.tools.clone(),
                inner.retry.clone(),
                st.model.clone(),
                st.thinking_level,
                st.temperature,
                st.response_format.clone(),
                st.active_tools.clone(),
                st.final_answer_mode.clone(),
                inner.max_tokens,
            )
        };

        // Wrap tools with HookedTool if before/after tool call hooks exist.
        let before = (!self.hooks.before_tool_call.is_empty()).then(|| {
            Arc::new(CompositeBeforeToolCallHook::new(
                self.hooks.before_tool_call.clone(),
            )) as Arc<dyn BeforeToolCallHook>
        });
        let after = (!self.hooks.after_tool_call.is_empty()).then(|| {
            Arc::new(CompositeAfterToolCallHook::new(
                self.hooks.after_tool_call.clone(),
            )) as Arc<dyn AfterToolCallHook>
        });
        let tools: Vec<Arc<dyn Tool>> = if before.is_some() || after.is_some() {
            let wrapped: Vec<Arc<dyn Tool>> = tools
                .into_iter()
                .map(|t| {
                    Arc::new(HookedTool {
                        inner: t,
                        before: before.clone(),
                        after: after.clone(),
                    }) as Arc<dyn Tool>
                })
                .collect();
            // Persist wrapped tools to state so that `DefaultPrepareNextTurn`
            // returns them on subsequent turns, preserving before/after hooks
            // across the agent loop.
            {
                let mut inner = self.inner.lock().unwrap();
                inner.state.tools.clone_from(&wrapped);
            }
            wrapped
        } else {
            tools
        };

        // Build the default prepare_next_turn wrapper.
        let prepare_next_turn: Arc<dyn PrepareNextTurnHook> = Arc::new(DefaultPrepareNextTurn {
            inner: self.inner.clone(),
            user_hook: (!self.hooks.prepare_next_turn.is_empty()).then(|| {
                Arc::new(CompositePrepareNextTurnHook::new(
                    self.hooks.prepare_next_turn.clone(),
                )) as Arc<dyn PrepareNextTurnHook>
            }),
        });

        LoopConfig {
            run,
            model,
            max_tokens,
            temperature,
            thinking_level,
            tools,
            active_tools,
            default_execution_mode: ToolExecutionMode::Parallel,
            final_answer_mode,
            env: self.env.clone(),
            abort,
            stream_options: self.stream_options.clone(),
            convert_to_llm: self.convert_to_llm.clone(),
            transform_context: (!self.hooks.transform_context.is_empty()).then(|| {
                Arc::new(CompositeTransformContextHook::new(
                    self.hooks.transform_context.clone(),
                )) as Arc<dyn TransformContextHook>
            }),
            prepare_next_turn: Some(prepare_next_turn),
            should_stop: (!self.hooks.should_stop.is_empty()).then(|| {
                Arc::new(CompositeShouldStopHook::new(self.hooks.should_stop.clone()))
                    as Arc<dyn ShouldStopHook>
            }),
            before_provider_request: (!self.hooks.before_provider_request.is_empty()).then(|| {
                Arc::new(CompositeBeforeProviderRequestHook::new(
                    self.hooks.before_provider_request.clone(),
                )) as Arc<dyn BeforeProviderRequestHook>
            }),
            after_provider_response: (!self.hooks.after_provider_response.is_empty()).then(|| {
                Arc::new(CompositeAfterProviderResponseHook::new(
                    self.hooks.after_provider_response.clone(),
                )) as Arc<dyn AfterProviderResponseHook>
            }),
            final_answer_validator: (!self.hooks.final_answer_validator.is_empty()).then(|| {
                Arc::new(CompositeFinalAnswerValidator::new(
                    self.hooks.final_answer_validator.clone(),
                )) as Arc<dyn FinalAnswerValidator>
            }),
            steer_rx: Some(steer_rx),
            follow_up_rx: Some(follow_up_rx),
            retry,
            response_format,
        }
    }

    pub(super) async fn run_loop(
        &self,
        run: Arc<RunContext>,
        initial: Vec<AgentMessage>,
    ) -> Result<(), HarnessError> {
        // 1. Setup: channels, abort token, system prompt, hooks.
        let (steer_rx, follow_up_rx, abort, system_prompt, original_tools, async_hook, watcher) = {
            let mut inner = self.inner.lock().unwrap();
            inner.state.streaming_message = None;
            inner.state.pending_tool_calls.clear();
            let (steer_rx, follow_up_rx) = inner.reset_channels();
            let abort = inner.current_abort.take().unwrap_or_default();
            inner.current_abort = Some(abort.clone());
            let sp = inner.state.system_prompt.clone();
            // clone Vec<Arc<dyn Tool>>（浅拷贝指针，非工具本体），用于 loop 结束后
            // 恢复未包装的 tools，避免下次 build_loop_config 二次包装 HookedTool。
            let ot = inner.state.tools.clone();
            let async_hook = inner.async_spawn_hook.clone();
            let watcher = inner.idle_watcher.clone();
            (steer_rx, follow_up_rx, abort, sp, ot, async_hook, watcher)
        };
        let _guard = self.enter_phase(HarnessPhase::Turning)?;
        if let Some(h) = &async_hook {
            h.borrow_completion_rx();
        }

        let result = self
            .drive_loop(run, initial, steer_rx, follow_up_rx, abort, system_prompt)
            .await;

        // Cleanup always runs, success or failure.
        {
            let mut inner = self.inner.lock().unwrap();
            inner.state.streaming_message = None;
            inner.state.pending_tool_calls.clear();
            inner.active_tool_names.clear();
            inner.current_abort = None;
            // On error, the run is discarded: reset the mid-run flag so the
            // selective restore below rolls back to original_tools.
            if let Err(ref e) = result {
                inner.state.error_message = Some(e.to_string());
                inner.state.pending_session_writes.clear();
                inner.state.tools_overwritten_mid_run = false;
            }
            // Restore original unwrapped tools so the next call to `build_loop_config`
            // doesn't re-wrap already-HookedTool-wrapped tools (double-wrapping) —
            // unless `set_tools` overwrote the tools mid-run, in which case keep the
            // new (unwrapped) set; next `build_loop_config` will wrap them as needed.
            if inner.state.tools_overwritten_mid_run {
                inner.state.tools_overwritten_mid_run = false;
            } else {
                inner.state.tools = original_tools;
            }
        }
        // Release the completion_rx borrow BEFORE dropping the guard (which
        // restores Idle). Otherwise a concurrent caller seeing Idle could
        // borrow_completion_rx before the previous run released it.
        if let Some(h) = &async_hook {
            h.release_completion_rx();
        }
        drop(_guard);
        // Idle 后：若配置了 watcher，让其检查 in-flight 并按需启动
        if let Some(w) = &watcher {
            w.on_idle();
        }

        result
    }

    async fn drive_loop(
        &self,
        run: Arc<RunContext>,
        initial: Vec<AgentMessage>,
        steer_rx: mpsc::Receiver<AgentMessage>,
        follow_up_rx: mpsc::Receiver<AgentMessage>,
        abort: CancellationToken,
        system_prompt: Option<String>,
    ) -> Result<(), HarnessError> {
        // 2. Build context from session.
        let built = self.session.build_context().await?;
        let messages: Vec<AgentMessage> =
            built.messages.into_iter().chain(initial.clone()).collect();

        let ctx = AgentContext {
            system_prompt,
            messages,
        };

        // 3. Build LoopConfig.
        let config = self.build_loop_config(run.clone(), steer_rx, follow_up_rx, abort.clone());

        // 4. Drive the stream. Push initial messages to pending before the loop starts
        //    (AgentStart is a signal-only event; initial messages are persisted here).
        for msg in &initial {
            self.push_pending_write(SessionEntryPayload::Message(msg.clone()));
        }

        let mut stream = Box::pin(agent_loop(self.client.clone(), ctx, config));
        let mut turn_messages: Vec<AgentMessage> = vec![];

        while let Some(event) = stream.next().await {
            match &event {
                AgentEvent::AgentStart => {
                    self.emit(AgentHarnessEvent::Agent(event));
                }
                AgentEvent::TurnStart { index } => {
                    if !self.hooks.before_turn.is_empty() {
                        let composite =
                            Arc::new(CompositeBeforeTurnHook::new(self.hooks.before_turn.clone()));
                        let snapshot = {
                            let inner = self.inner.lock().unwrap();
                            TurnSnapshot {
                                model: inner.state.model.clone(),
                                thinking_level: inner.state.thinking_level,
                                temperature: inner.state.temperature,
                                tools: inner.state.tools.clone(),
                                system_prompt: inner.state.system_prompt.clone(),
                            }
                        };
                        composite
                            .before_turn(BeforeTurnCtx {
                                run: &run,
                                turn_index: *index,
                                snapshot: &snapshot,
                            })
                            .await;
                    }
                    turn_messages.clear();
                    self.emit(AgentHarnessEvent::Agent(event));
                }
                AgentEvent::MessageUpdate { partial, .. } => {
                    self.inner.lock().unwrap().state.streaming_message = Some(partial.clone());
                    self.emit(AgentHarnessEvent::Agent(event));
                }
                AgentEvent::MessageEnd {
                    message, discarded, ..
                } => {
                    self.inner.lock().unwrap().state.streaming_message = None;
                    if !discarded {
                        let msg = AgentMessage::Assistant(message.clone());
                        self.push_pending_write(SessionEntryPayload::Message(msg.clone()));
                        turn_messages.push(msg);
                    }
                    self.emit(AgentHarnessEvent::Agent(event));
                }
                AgentEvent::ToolExecutionStart {
                    tool_use_id,
                    tool_name,
                    args,
                } => {
                    {
                        let mut inner = self.inner.lock().unwrap();
                        inner.state.pending_tool_calls.insert(tool_use_id.clone());
                        inner
                            .active_tool_names
                            .insert(tool_use_id.clone(), tool_name.clone());
                    }
                    self.emit(AgentHarnessEvent::ToolCallStart {
                        tool_use_id: tool_use_id.clone(),
                        tool_name: tool_name.clone(),
                        args: args.clone(),
                    });
                    self.emit(AgentHarnessEvent::Agent(event));
                }
                AgentEvent::ToolExecutionEnd {
                    tool_use_id,
                    result,
                } => {
                    self.inner
                        .lock()
                        .unwrap()
                        .state
                        .pending_tool_calls
                        .remove(tool_use_id.as_str());

                    // Build ToolResultMessage for session persistence.
                    let is_error = result.is_err();
                    let timestamp = chrono::Utc::now();
                    let msg = match result {
                        Ok(result) => result.session_message(tool_use_id.clone(), timestamp),
                        Err(failure) => failure.session_message(tool_use_id.clone(), timestamp),
                    }
                    .map(AgentMessage::ToolResult)
                    .map_err(|error| AgentError::Internal(error.to_string()))?;
                    self.push_pending_write(SessionEntryPayload::Message(msg.clone()));
                    turn_messages.push(msg);

                    // Emit ToolCallEnd harness event.
                    let details = match result {
                        Ok(r) => r.details.clone(),
                        Err(failure) => failure.details.clone(),
                    };
                    let resolved_name = self
                        .inner
                        .lock()
                        .unwrap()
                        .active_tool_names
                        .remove(tool_use_id.as_str())
                        .unwrap_or_default();
                    self.emit(AgentHarnessEvent::ToolCallEnd {
                        tool_use_id: tool_use_id.clone(),
                        tool_name: resolved_name,
                        result: HarnessToolCallResult { details, is_error },
                    });
                    self.emit(AgentHarnessEvent::Agent(event));
                }
                AgentEvent::TurnEnd { index, .. } => {
                    // Call after_turn hook before flush.
                    if !self.hooks.after_turn.is_empty() {
                        let composite =
                            Arc::new(CompositeAfterTurnHook::new(self.hooks.after_turn.clone()));
                        composite
                            .after_turn(AfterTurnCtx {
                                run: &run,
                                turn_index: *index,
                                new_messages: &turn_messages,
                            })
                            .await;
                    }
                    // Flush pending writes (save point).
                    let flushed = self.flush_pending_writes().await?;
                    self.emit(AgentHarnessEvent::SavePoint {
                        entries_flushed: flushed,
                    });
                    self.emit(AgentHarnessEvent::Agent(event));
                }
                AgentEvent::AgentEnd { .. } => {
                    self.emit(AgentHarnessEvent::Agent(event));
                    self.emit(AgentHarnessEvent::Settled);
                    break;
                }
                AgentEvent::Error(e) => {
                    // Clear pending writes so the failed turn's user message is not
                    // flushed to the session on a future successful turn, which would
                    // produce two consecutive user messages and cause Anthropic to reject.
                    {
                        let mut inner = self.inner.lock().unwrap();
                        inner.state.streaming_message = None;
                        inner.state.pending_session_writes.clear();
                    }
                    if matches!(e, AgentError::Aborted) {
                        // Abort is a terminal sentinel, not a failure: emit Aborted
                        // (mirroring Settled) so consumers can distinguish cancel from
                        // a channel-closed crash. drive_loop returns Ok — the abort is
                        // a signal, not a propagated error — so error_message must NOT
                        // be set (it is a pub field exposed via state()).
                        self.emit(AgentHarnessEvent::Aborted);
                        break;
                    }
                    {
                        let mut inner = self.inner.lock().unwrap();
                        inner.state.error_message = Some(e.to_string());
                    }
                    self.emit(AgentHarnessEvent::Agent(event));
                }
                _ => {
                    self.emit(AgentHarnessEvent::Agent(event));
                }
            }
        }

        Ok(())
    }
}
