// loop_driver.rs — loop driving methods extracted from harness/mod.rs
use std::sync::Arc;

use futures::{FutureExt, StreamExt};
use llm_harness_loop::CompositeAfterRunHook;
use llm_harness_loop::CompositePrepareNextTurnHook;
use llm_harness_loop::{
    CompositeAfterProviderResponseHook, CompositeAfterToolCallHook, CompositeAfterTurnHook,
    CompositeBeforeProviderRequestHook, CompositeBeforeRunHook, CompositeBeforeToolCallHook,
    CompositeBeforeTurnHook, CompositeFinalAnswerValidator, CompositeProviderErrorHook,
    CompositeShouldStopHook, CompositeTransformContextHook, HookedTool, LoopConfig, agent_loop,
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
        // Idempotency: skip if initial already contains a skills metadata
        // message (e.g. continue_run re-entering before_run after a previous
        // injection in the same run, or additional_messages from a hook).
        if has_skill_readiness {
            let already_injected = initial.iter().any(|m| {
                matches!(m, AgentMessage::User(u) if u.content.iter().any(|c| {
                    matches!(c, ContentBlock::Text { text } if text.starts_with("<available-skills>"))
                }))
            });
            if !already_injected {
                let metadata = crate::skills::format_skills_metadata(&resources.skills, true);
                if !metadata.is_empty() {
                    initial.push(AgentMessage::User(UserMessage {
                        content: vec![ContentBlock::Text { text: metadata }],
                        timestamp: chrono::Utc::now(),
                    }));
                }
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
            default_execution_mode,
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
                st.default_execution_mode,
                inner.max_tokens,
            )
        };

        // Wrap tools with HookedTool if before/after tool call hooks exist,
        // or any tool opts into duration reporting (the decorator owns both).
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
        let tools: Vec<Arc<dyn Tool>> =
            if before.is_some() || after.is_some() || tools.iter().any(|t| t.report_duration()) {
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
            default_execution_mode,
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
            provider_error: (!self.hooks.provider_error.is_empty()).then(|| {
                Arc::new(CompositeProviderErrorHook::new(
                    self.hooks.provider_error.clone(),
                )) as Arc<dyn ProviderErrorHook>
            }),
            // Hook-driven same-turn resends are hard-capped at 1 per turn:
            // the adapter cannot yet distinguish a trusted 400 input
            // rejection from an unknown-outcome gateway failure folded into
            // `InvalidRequest`, so an unbounded repair retry would replay
            // the request.
            max_provider_error_retries: None,
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
        // 1. Setup: channels, abort token, system prompt, original tools.
        let (mut steer_rx, follow_up_rx, abort, system_prompt, original_tools) = {
            let mut inner = self.inner.lock().unwrap();
            inner.state.streaming_message = None;
            inner.state.pending_tool_calls.clear();
            let (steer_rx, follow_up_rx) = inner.reset_channels();
            let abort = inner.current_abort.take().unwrap_or_default();
            inner.current_abort = Some(abort.clone());
            let sp = inner.state.system_prompt.clone();
            let ot = inner.state.tools.clone();
            (steer_rx, follow_up_rx, abort, sp, ot)
        };
        let _guard = self.enter_phase(HarnessPhase::Turning)?;

        // AssertUnwindSafe: state is Arc-backed shared state that stays
        // consistent across a panic (see agent_runner.rs catch_unwind).
        // Catching here guarantees state cleanup ALWAYS runs — the failure
        // path in the block below, the panic path in the Err arm. The Err
        // arm must clean up BEFORE re-raising: resume_unwind never returns,
        // so anything after the match is skipped on that path. Skipping
        // cleanup leaves state.tools holding the HookedTool wrappers and
        // the next build_loop_config (panic-retry paths) double-wraps,
        // double-firing hooks and appending duplicate duration annotations.
        let result = match std::panic::AssertUnwindSafe(self.drive_loop(
            run,
            initial,
            &mut steer_rx,
            follow_up_rx,
            abort,
            system_prompt,
        ))
        .catch_unwind()
        .await
        {
            Ok(result) => result,
            Err(panic_payload) => {
                // resume_unwind never returns, so the shared cleanup block
                // after this match is unreachable on the panic path. Restore
                // state HERE — critically `tools`, whose wrapped value would
                // make the next build_loop_config (agent-team's catch_unwind
                // retry) double-wrap and double-fire hooks — then re-raise so
                // callers observe the panic as before. Unlike the Err(result)
                // branch, buffered session writes are KEPT: the panicked
                // run's transcript entries must still flush (pinned by
                // agent-team's run_panic_returns_messages_and_recovers_to_idle).
                {
                    let mut inner = self.inner.lock().unwrap();
                    inner.state.streaming_message = None;
                    inner.state.pending_tool_calls.clear();
                    inner.active_tool_names.clear();
                    inner.current_abort = None;
                    inner.state.tools_overwritten_mid_run = false;
                    inner.state.tools = original_tools;
                }
                std::panic::resume_unwind(panic_payload);
            }
        };

        // Cleanup always runs, success or failure.
        {
            let mut inner = self.inner.lock().unwrap();
            // Salvage steers the loop never consumed: agent_loop drains the
            // steer channel one last time before it stops, but a steer sent
            // after that final drain (e.g. the agent-team monitor racing the
            // run's exit) still returns Ok from try_send and then dies with
            // the receiver here. Buffer the leftovers as next-turn messages
            // so the next run injects them instead of losing them silently.
            // Once drive_loop has returned nothing else reads this receiver,
            // so this drain owns whatever remains.
            let salvaged = std::mem::take(&mut inner.state.queued_next_turn);
            if !salvaged.is_empty() {
                inner.state.salvaged_steers = true;
                inner.state.queued_next_turn = salvaged;
            }
            while let Ok(leftover) = steer_rx.try_recv() {
                inner.state.queued_next_turn.push(leftover);
                inner.state.salvaged_steers = true;
            }
            inner.state.streaming_message = None;
            inner.state.pending_tool_calls.clear();
            inner.active_tool_names.clear();
            inner.current_abort = None;
            if let Err(ref e) = result {
                inner.state.error_message = Some(e.to_string());
                inner.state.pending_session_writes.clear();
                inner.state.tools_overwritten_mid_run = false;
            }
            if inner.state.tools_overwritten_mid_run {
                inner.state.tools_overwritten_mid_run = false;
            } else {
                inner.state.tools = original_tools;
            }
        }
        drop(_guard); // Restore Idle.

        // AfterRunHook: replaces watcher.on_idle().
        if !self.hooks.after_run.is_empty() {
            let composite = Arc::new(CompositeAfterRunHook::new(self.hooks.after_run.clone()));
            composite.after_run().await;
        }

        result
    }

    async fn drive_loop(
        &self,
        run: Arc<RunContext>,
        initial: Vec<AgentMessage>,
        steer_rx: &mut mpsc::Receiver<AgentMessage>,
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

        // 3. Build LoopConfig. The loop owns the receiver for its drains;
        // the caller's binding gets a placeholder until the stream ends,
        // then the real receiver is moved back so run_loop's salvage can
        // recover any steer the loop never consumed.
        let (_orphan_tx, orphan_rx) = mpsc::channel(1);
        let real_rx = std::mem::replace(steer_rx, orphan_rx);
        let config = self.build_loop_config(run.clone(), real_rx, follow_up_rx, abort.clone());

        // 4. Drive the stream. Push initial messages to pending before the loop starts
        //    (AgentStart is a signal-only event; initial messages are persisted here).
        for msg in &initial {
            self.push_pending_write(SessionEntryPayload::Message(msg.clone()));
        }

        let mut stream = Box::pin(agent_loop(self.client(), ctx, config));
        let mut turn_messages: Vec<AgentMessage> = vec![];

        while let Some(event) = stream.next().await {
            if matches!(
                &event,
                AgentEvent::MessageStart { .. } | AgentEvent::ToolExecutionStart { .. }
            ) {
                let mut inner = self.inner.lock().unwrap();
                inner.state.replay_has_progress = true;
                if matches!(&event, AgentEvent::ToolExecutionStart { .. }) {
                    inner.state.replay_tool_started = true;
                }
            }
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
                    // The generator may still yield a final SteerOrphaned
                    // (steers that died with the channel). Pull the tail so
                    // the salvage is honored before the receiver drops.
                    while let Some(trailing) = stream.next().await {
                        if let AgentEvent::SteerOrphaned { messages } = &trailing {
                            let mut inner = self.inner.lock().unwrap();
                            inner.state.queued_next_turn.extend(messages.clone());
                            inner.state.salvaged_steers = true;
                        }
                        self.emit(AgentHarnessEvent::Agent(trailing));
                    }
                    break;
                }
                AgentEvent::SteerOrphaned { messages } => {
                    // Steers the loop never consumed (teardown race, abort).
                    // Buffer for the next run and flag the host scheduler so
                    // it can requeue the originals if its wakeup is inbox-only.
                    let mut inner = self.inner.lock().unwrap();
                    inner.state.queued_next_turn.extend(messages.clone());
                    inner.state.salvaged_steers = true;
                    self.emit(AgentHarnessEvent::Agent(event));
                }
                AgentEvent::SteerConsumed { messages } => {
                    // Steers injected at a turn boundary: persist them so the
                    // session transcript matches what the model actually saw.
                    for msg in messages {
                        self.push_pending_write(SessionEntryPayload::Message(msg.clone()));
                    }
                    self.emit(AgentHarnessEvent::Agent(event));
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
                        // Keep pulling: the generator still owes AgentEnd and
                        // possibly a SteerOrphaned salvage; the AgentEnd arm
                        // below drains the tail and breaks.
                        continue;
                    }
                    {
                        let mut inner = self.inner.lock().unwrap();
                        inner.state.error_message = Some(e.to_string());
                        inner.state.last_error = Some(e.clone());
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
