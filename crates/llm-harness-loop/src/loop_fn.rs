use std::{collections::HashSet, sync::Arc};

use futures::{Stream, StreamExt};
use llm_adapter::LlmError;
use llm_adapter::provider::Provider;
use llm_harness_types::*;
use uuid::Uuid;

use crate::config::LoopConfig;
use crate::final_answer::{
    FinalAnswerMissingBehavior, FinalAnswerMode, FinalAnswerToolConfig, builtin_final_answer_tool,
};
use crate::stream_state::TerminalTextClassification;

type ToolSelection = (Vec<Arc<dyn Tool>>, Option<FinalAnswerToolConfig>);

/// Convert an `LlmError` into a typed `AgentError::ProviderTyped`,
/// preserving the error variant information for SDK layers.
///
/// Note: this matches exhaustively against the external `llm_adapter::LlmError`
/// enum. A new variant there intentionally fails compilation here so the map is
/// kept complete rather than silently degrading new errors to `Other`.
fn llm_error_to_agent_error(e: LlmError) -> AgentError {
    let kind = match &e {
        LlmError::InvalidRequest(msg) => ProviderErrorKind::InvalidRequest(msg.clone()),
        LlmError::Unauthorized => ProviderErrorKind::Unauthorized,
        LlmError::Forbidden => ProviderErrorKind::Forbidden,
        LlmError::RateLimit { retry_after } => ProviderErrorKind::RateLimit {
            retry_after: *retry_after,
        },
        LlmError::Overloaded { retry_after } => ProviderErrorKind::Overloaded {
            retry_after: *retry_after,
        },
        LlmError::ServerError(msg) => ProviderErrorKind::ServerError(msg.clone()),
        LlmError::Timeout => ProviderErrorKind::Timeout,
        LlmError::Network(_) => ProviderErrorKind::Network,
        LlmError::Stream(msg) => ProviderErrorKind::Stream(msg.clone()),
        LlmError::StreamIncomplete {
            received_chunks,
            finish_reason,
        } => ProviderErrorKind::StreamIncomplete {
            received_chunks: *received_chunks,
            finish_reason: finish_reason.clone(),
        },
        LlmError::Decode(_) => ProviderErrorKind::Decode(e.to_string()),
        LlmError::Provider { code, .. } => ProviderErrorKind::Other { code: code.clone() },
    };
    AgentError::ProviderTyped {
        message: e.to_string(),
        kind,
    }
}

fn active_tool_subset(
    tools: &[Arc<dyn Tool>],
    active: &Option<HashSet<String>>,
) -> Result<Vec<Arc<dyn Tool>>, AgentError> {
    let Some(active) = active else {
        return Ok(tools.to_vec());
    };

    if let Some(name) = active
        .iter()
        .find(|name| !tools.iter().any(|tool| tool.name() == name.as_str()))
    {
        return Err(AgentError::Internal(format!(
            "active tool '{name}' is not registered"
        )));
    }

    Ok(tools
        .iter()
        .filter(|tool| active.contains(tool.name()))
        .cloned()
        .collect())
}

fn final_answer_tool_config(mode: &FinalAnswerMode) -> Option<&FinalAnswerToolConfig> {
    match mode {
        FinalAnswerMode::Heuristic => None,
        FinalAnswerMode::Tool(cfg) => Some(cfg),
    }
}

fn tools_for_turn(
    tools: &[Arc<dyn Tool>],
    active: &Option<HashSet<String>>,
    final_answer_mode: &FinalAnswerMode,
) -> Result<ToolSelection, AgentError> {
    let Some(answer_cfg) = final_answer_tool_config(final_answer_mode).cloned() else {
        return active_tool_subset(tools, active).map(|tools| (tools, None));
    };

    let builtin = builtin_final_answer_tool();
    let mut registered = tools.to_vec();
    registered.push(builtin.clone());

    let mut selected = active_tool_subset(&registered, active)?;
    if !selected
        .iter()
        .any(|tool| tool.name() == FINAL_ANSWER_TOOL_NAME)
    {
        selected.push(builtin);
    }

    Ok((selected, Some(answer_cfg)))
}

fn terminal_text_classification(mode: &FinalAnswerMode) -> TerminalTextClassification {
    match mode {
        FinalAnswerMode::Tool(cfg) if cfg.missing_behavior == FinalAnswerMissingBehavior::Error => {
            TerminalTextClassification::Progress
        }
        FinalAnswerMode::Heuristic | FinalAnswerMode::Tool(_) => {
            TerminalTextClassification::FinalAnswer
        }
    }
}

fn final_answer_text_from_results(
    tool_name: &str,
    tool_calls: &[(String, String, serde_json::Value)],
    results: &[(String, Result<ToolResult, ToolFailure>)],
) -> Option<String> {
    tool_calls
        .iter()
        .filter(|(_, name, _)| name == tool_name)
        .filter_map(|(id, _, _)| {
            results.iter().find_map(|(result_id, result)| {
                if result_id != id {
                    return None;
                }
                match result {
                    Ok(tool_result) => tool_result
                        .details
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    Err(_) => None,
                }
            })
        })
        .next_back()
}

async fn validate_final_answer(
    config: &LoopConfig,
    candidate: &AssistantMessage,
    turn_index: u32,
) -> Result<(), AgentError> {
    let Some(validator) = &config.final_answer_validator else {
        return Ok(());
    };
    validator
        .validate(FinalAnswerValidationCtx {
            run: &config.run,
            candidate,
            turn_index,
        })
        .await
        .map_err(|error| AgentError::FinalAnswerRejected {
            code: error.code,
            message: error.message,
        })
}

fn apply_next_turn_directive(
    ctx: &mut AgentContext,
    config: &mut LoopConfig,
    active_tools: &mut Option<HashSet<String>>,
    directive: NextTurnDirective,
) -> Result<(), AgentError> {
    if let Some(new_ctx) = directive.context {
        *ctx = new_ctx;
    }
    if let Some(model) = directive.model {
        config.model = model;
    }
    if let Some(level) = directive.thinking_level {
        config.thinking_level = level;
    }
    if let Some(temp) = directive.temperature {
        config.temperature = temp;
    }
    if let Some(tools) = directive.tools {
        config.tools = tools;
    }
    if let Some(rf) = directive.response_format {
        config.response_format = rf;
    }
    *active_tools = directive.active_tools;
    tools_for_turn(&config.tools, active_tools, &config.final_answer_mode).map(|_| ())
}

/// Start an agent loop from a new context.
pub fn agent_loop(
    client: Arc<dyn Provider>,
    ctx: AgentContext,
    config: LoopConfig,
) -> impl Stream<Item = AgentEvent> + Send {
    run_loop(client, ctx, config, false)
}

/// Continue an agent loop without injecting new initial messages.
///
/// Drains stale steer messages (already queued before this call) before the first LLM call.
pub fn agent_loop_continue(
    client: Arc<dyn Provider>,
    ctx: AgentContext,
    config: LoopConfig,
) -> impl Stream<Item = AgentEvent> + Send {
    run_loop(client, ctx, config, true)
}

fn run_loop(
    client: Arc<dyn Provider>,
    mut ctx: AgentContext,
    mut config: LoopConfig,
    drain_steer_first: bool,
) -> impl Stream<Item = AgentEvent> + Send {
    async_stream::stream! {
        use crate::{
            dispatch::execute_tool_batch,
            stream_state::StreamingState,
            type_bridge::tools_to_defs,
        };
        use llm_adapter::types::{ChatRequest, Message as AMsg, ToolChoice};
        use tokio::sync::mpsc;

        yield AgentEvent::AgentStart;

        // Drain stale steer messages for agent_loop_continue
        if drain_steer_first && let Some(ref mut rx) = config.steer_rx {
            while rx.try_recv().is_ok() {}
        }

        let mut turn_index: u32 = 0;
        let mut new_messages: Vec<AgentMessage> = vec![];
        let mut active_tools = config.active_tools.clone();

        'main: loop {
            // Check abort
            if config.abort.is_cancelled() {
                yield AgentEvent::Error(AgentError::Aborted);
                break 'main;
            }

            // Drain steer messages between turns
            if turn_index > 0 && let Some(ref mut rx) = config.steer_rx {
                while let Ok(msg) = rx.try_recv() {
                    ctx.messages.push(msg.clone());
                    new_messages.push(msg);
                }
            }

            // TransformContextHook
            if let Some(h) = &config.transform_context {
                match h
                    .transform(TransformContextCtx {
                        run: &config.run,
                        turn_index,
                        abort: config.abort.clone(),
                        context: ctx,
                    })
                    .await
                {
                    Ok(new_ctx) => ctx = new_ctx,
                    Err(e) => {
                        yield AgentEvent::Error(e);
                        break 'main;
                    }
                }
            }

            let turn_id = Uuid::now_v7().to_string();
            yield AgentEvent::TurnStart { index: turn_index };

            // Build message list, prepending system message if present.
            let mut llm_messages = match config.convert_to_llm.convert(&ctx.messages).await {
                Ok(m) => m,
                Err(e) => { yield AgentEvent::Error(e); break 'main; }
            };
            if let Some(sp) = &ctx.system_prompt {
                llm_messages.insert(0, AMsg::System(sp.clone()));
            }

            let (current_tools, current_final_answer_tool) = match tools_for_turn(
                &config.tools,
                &active_tools,
                &config.final_answer_mode,
            ) {
                Ok(result) => result,
                Err(e) => {
                    yield AgentEvent::Error(e);
                    break 'main;
                }
            };
            let tool_defs = tools_to_defs(&current_tools);
            let tool_choice =
                if tool_defs.is_empty() { ToolChoice::None } else { ToolChoice::Auto };

            let mut req_b = ChatRequest::builder(&config.model, config.max_tokens)
                .messages(llm_messages)
                .tools(tool_defs)
                .tool_choice(tool_choice);
            if let Some(t) = config.temperature {
                req_b = req_b.temperature(t);
            }
            // harness ThinkingLevel 现在就是 adapter ThinkingLevel，直接传。
            // 无条件注入：Off → Some(Off) 显式禁用（DeepSeek 注入 disabled），
            // 其他档位按 provider converter 翻译为 wire format。
            req_b = req_b.thinking(config.thinking_level);
            if let Some(fmt) = &config.response_format {
                req_b = req_b.response_format(fmt.clone());
            }
            let req = req_b.build();

            // BeforeProviderRequestHook: allow callers to modify stream options.
            let mut stream_opts = config.stream_options.clone();
            if let Some(h) = &config.before_provider_request {
                h.before_request(BeforeProviderRequestCtx {
                    run: &config.run,
                    turn_index,
                    options: &mut stream_opts,
                })
                .await;
            }

            // Call LLM (with optional retry on transient errors).
            // The retry loop encompasses both connection establishment AND
            // stream event consumption. An idle watchdog fires if no
            // StreamEvent arrives within stream_idle_timeout_ms, dropping
            // the StreamHandle and retrying the whole request.
            use crate::config::{cap_delay, is_retryable, RetryConfig};

            let mut retry = config.retry.as_ref().cloned().unwrap_or(RetryConfig {
                max_retries: 0,
                base_delay_ms: 0,
            });
            if let Some(max_retries) = stream_opts.max_retries {
                retry.max_retries = max_retries;
            }
            let mut attempt = 0u32;

            let (assistant_msg, handle_status_code, handle_latency_ms) = loop {
                // === Connection establishment ===
                let call = client.chat_stream(&req);
                let response = if let Some(timeout_ms) = stream_opts.timeout_ms {
                    match tokio::time::timeout(
                        std::time::Duration::from_millis(timeout_ms),
                        call,
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => {
                            yield AgentEvent::Error(AgentError::ProviderTyped {
                                message: format!("provider request timed out after {timeout_ms}ms"),
                                kind: ProviderErrorKind::Timeout,
                            });
                            break 'main;
                        }
                    }
                } else {
                    call.await
                };

                let mut handle = match response {
                    Ok(h) => h,
                    Err(e) if is_retryable(&e) && retry.can_retry(attempt) => {
                        let delay = cap_delay(
                            retry.delay_for(attempt, &e),
                            stream_opts.max_retry_delay_ms,
                        );
                        yield AgentEvent::RetryAttempt {
                            attempt,
                            max_retries: retry.max_retries,
                            delay_ms: delay.as_millis() as u64,
                            error: e.to_string(),
                        };
                        tokio::select! {
                            biased;
                            _ = config.abort.cancelled() => {
                                yield AgentEvent::Error(AgentError::Aborted);
                                break 'main;
                            }
                            _ = tokio::time::sleep(delay) => {}
                        }
                        attempt += 1;
                        continue;
                    }
                    Err(e) => {
                        yield AgentEvent::Error(llm_error_to_agent_error(e));
                        break 'main;
                    }
                };

                // === Stream consumption with idle watchdog ===
                let mut state = StreamingState::new_with_terminal_text_classification(
                    handle.model().to_owned(),
                    turn_id.clone(),
                    terminal_text_classification(&config.final_answer_mode),
                );
                let message_id = state.message_id.clone();
                // Tracks whether any consumer-visible AgentEvent was emitted for
                // this message (MessageStart, TextDelta, MessageUpdate, ToolCall*,
                // …). MessageStart itself counts — if the stream stalls after
                // only ContentStart (which produces no AgentEvent), we still
                // need to close the orphaned MessageStart with a discard
                // MessageEnd before retrying.
                let mut had_visible_events = true;
                yield AgentEvent::MessageStart { message_id };

                // Idle watchdog: a single reusable Sleep hoisted above the
                // consume loop. Reset on every event arrival to re-arm.
                // Some(0) is treated as disabled (avoid busy-retry spin).
                let idle_timeout = stream_opts.stream_idle_timeout_ms.filter(|&ms| ms > 0);
                // Always use a Sleep; when disabled, deadline is far future (never fires).
                let idle_sleep = tokio::time::sleep(match idle_timeout {
                    Some(ms) => std::time::Duration::from_millis(ms),
                    None => std::time::Duration::MAX,
                });
                tokio::pin!(idle_sleep);

                let mut final_message: Option<AssistantMessage> = None;
                let stream_result: Result<Option<AssistantMessage>, AgentError> = loop {
                    let event_fut = handle.events().next();
                    tokio::pin!(event_fut);

                    tokio::select! {
                        biased;
                        _ = config.abort.cancelled() => {
                            break Err(AgentError::Aborted);
                        }
                        _ = &mut idle_sleep => {
                            // Idle timeout fired — break out of consume loop.
                            // handle is dropped below, outside select!.
                            break Err(AgentError::StreamIdle {
                                timeout_ms: idle_timeout.unwrap(),
                            });
                        }
                        ev = &mut event_fut => {
                            match ev {
                                Some(Ok(adapter_event)) => {
                                    let is_terminal = matches!(
                                        &adapter_event,
                                        llm_adapter::types::StreamEvent::MessageStop { .. }
                                    );
                                    for agent_event in state.process(adapter_event) {
                                        if let AgentEvent::MessageEnd { ref message, .. } = agent_event {
                                            final_message = Some(message.clone());
                                            if message.kind == AssistantMessageKind::FinalAnswer {
                                                continue;
                                            }
                                        }
                                        had_visible_events = true;
                                        yield agent_event;
                                    }
                                    // Re-arm idle watchdog on every event.
                                    if let Some(ms) = idle_timeout {
                                        idle_sleep.as_mut().reset(tokio::time::Instant::now()
                                            + std::time::Duration::from_millis(ms));
                                    }
                                    if is_terminal {
                                        break Ok(final_message);
                                    }
                                }
                                Some(Err(e)) => {
                                    break Err(llm_error_to_agent_error(e));
                                }
                                None => break Ok(final_message),
                            }
                        }
                    }
                };
                let handle_status_code = handle.status_code();
                let handle_latency_ms = handle.latency_ms();
                // handle dropped here (end of this loop iteration's scope).
                // This kills the underlying HTTP connection.
                drop(handle);

                match stream_result {
                    Ok(Some(msg)) => break (msg, handle_status_code, handle_latency_ms),
                    Ok(None) => {
                        yield AgentEvent::Error(AgentError::Internal(
                            "stream ended without MessageEnd".into(),
                        ));
                        break 'main;
                    }
                    Err(AgentError::Aborted) => {
                        yield AgentEvent::Error(AgentError::Aborted);
                        break 'main;
                    }
                    Err(e) => {
                        // Idle timeout or stream error — check retry.
                        let is_idle = matches!(e, AgentError::StreamIdle { .. });
                        if is_idle && retry.can_retry(attempt) {
                            // Close the orphaned partial message so consumers
                            // pairing MessageStart→MessageEnd see a clean
                            // boundary before the retry emits a new MessageStart.
                            if had_visible_events {
                                let partial = state.discarded_message();
                                yield AgentEvent::MessageEnd {
                                    message_id: partial.message_id.clone(),
                                    message: partial,
                                    discarded: true,
                                };
                            }
                            let delay = cap_delay(
                                retry.delay_for(attempt, &llm_adapter::LlmError::Timeout),
                                stream_opts.max_retry_delay_ms,
                            );
                            yield AgentEvent::RetryAttempt {
                                attempt,
                                max_retries: retry.max_retries,
                                delay_ms: delay.as_millis() as u64,
                                error: e.to_string(),
                            };
                            tokio::select! {
                                biased;
                                _ = config.abort.cancelled() => {
                                    yield AgentEvent::Error(AgentError::Aborted);
                                    break 'main;
                                }
                                _ = tokio::time::sleep(delay) => {}
                            }
                            attempt += 1;
                            continue;
                        } else {
                            yield AgentEvent::Error(e);
                            break 'main;
                        }
                    }
                }
            };

            // AfterProviderResponseHook: observation hook for quota tracking, cost monitoring.
            // Only reached on the successful path (Ok(Some(msg)) => break msg above).
            if let Some(h) = &config.after_provider_response {
                let info = ProviderResponseInfo {
                    status_code: handle_status_code,
                    response_headers: vec![],
                    usage: assistant_msg.usage.clone(),
                    latency_ms: handle_latency_ms,
                    model: assistant_msg.model.clone(),
                    provider: assistant_msg.provider.clone(),
                };
                h.after_response(AfterProviderResponseCtx {
                    run: &config.run,
                    turn_index,
                    info: &info,
                })
                .await;
            }

            if assistant_msg.kind == AssistantMessageKind::FinalAnswer {
                if let Err(error) =
                    validate_final_answer(&config, &assistant_msg, turn_index).await
                {
                    yield AgentEvent::Error(error);
                    break 'main;
                }
                yield AgentEvent::MessageEnd {
                    message_id: assistant_msg.message_id.clone(),
                    message: assistant_msg.clone(),
                    discarded: false,
                };
            }

            let assistant_agent_msg = AgentMessage::Assistant(assistant_msg.clone());
            ctx.messages.push(assistant_agent_msg.clone());
            new_messages.push(assistant_agent_msg);

            let stop_reason = assistant_msg.stop_reason.unwrap_or(StopReason::EndTurn);

            // Unconditionally extract tool-use blocks — providers may emit ToolUse
            // blocks with a non-ToolUse stop_reason (e.g. Anthropic structured output
            // returns tool_use + EndTurn). Leaving them orphaned causes a provider 400
            // on the next turn.
            let tool_calls: Vec<(String, String, serde_json::Value)> = assistant_msg
                .content
                .iter()
                .filter_map(|cb| {
                    if let ContentBlock::ToolUse { id, name, input } = cb {
                        Some((id.clone(), name.clone(), input.clone()))
                    } else {
                        None
                    }
                })
                .collect();

            if !tool_calls.is_empty() {
                if stop_reason != StopReason::ToolUse {
                    tracing::warn!(
                        tool_use_ids = ?tool_calls.iter().map(|(id, _, _)| id).collect::<Vec<_>>(),
                        stop_reason = ?stop_reason,
                        "assistant message contains ToolUse blocks but stop_reason is not ToolUse; \
                         proceeding with tool execution to preserve trajectory integrity"
                    );
                }

                // Match tool calls to registered tools.
                let tools_by_name: std::collections::HashMap<&str, Arc<dyn Tool>> = current_tools
                    .iter()
                    .map(|tool| (tool.name(), tool.clone()))
                    .collect();
                let calls: Vec<(String, serde_json::Value, Arc<dyn Tool>)> = tool_calls
                    .iter()
                    .filter_map(|(id, name, args)| {
                        tools_by_name
                            .get(name.as_str())
                            .map(|tool| (id.clone(), args.clone(), tool.clone()))
                    })
                    .collect();

                // Identify unregistered tool calls before moving `calls` into the executor.
                // Leaving orphan ToolUse blocks without a matching ToolResult would cause
                // a 400 error on the next provider request.
                let unmatched: Vec<(String, String)> = tool_calls
                    .iter()
                    .filter(|(id, _, _)| !calls.iter().any(|(cid, _, _)| cid == id))
                    .map(|(id, name, _)| (id.clone(), name.clone()))
                    .collect();

                // Emit ToolExecutionStart for each call
                for (id, name, args) in &tool_calls {
                    yield AgentEvent::ToolExecutionStart {
                        tool_use_id: id.clone(),
                        tool_name: name.clone(),
                        args: args.clone(),
                    };
                }

                let assistant_arc = Arc::new(assistant_msg.clone());
                let run = config.run.clone();
                let env = config.env.clone();
                let abort = config.abort.clone();
                let turn_idx = turn_index;

                // Aggregate per-tool update channels so partial tool results can be
                // tagged with their originating tool_use_id before being emitted.
                let (update_tx, mut update_rx) = mpsc::channel::<(String, ToolProgress)>(64);

                let batch = execute_tool_batch(
                    calls,
                    {
                        let env = env.clone();
                        let abort = abort.clone();
                        let assistant_arc = assistant_arc.clone();
                        let update_tx = update_tx.clone();
                        move |tool_use_id| {
                            let (tool_update_tx, mut tool_update_rx) =
                                mpsc::channel::<ToolProgress>(16);
                            let aggregate_tx = update_tx.clone();
                            let update_tool_use_id = tool_use_id.clone();
                            tokio::spawn(async move {
                                while let Some(partial) = tool_update_rx.recv().await {
                                    if aggregate_tx
                                        .send((update_tool_use_id.clone(), partial))
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                            });

                            ToolContext {
                                run: run.clone(),
                                env: env.clone(),
                                abort: abort.clone(),
                                tool_use_id,
                                turn_index: turn_idx,
                                assistant_message: assistant_arc.clone(),
                                update_tx: tool_update_tx,
                            }
                        }
                    },
                    config.default_execution_mode,
                );
                drop(update_tx);
                futures::pin_mut!(batch);

                let mut results = loop {
                    tokio::select! {
                        biased;

                        Some((tool_use_id, partial)) = update_rx.recv() => {
                            yield AgentEvent::ToolExecutionUpdate {
                                tool_use_id,
                                partial,
                            };
                        }
                        batch_results = &mut batch => {
                            break batch_results;
                        }
                    }
                };
                while let Some((tool_use_id, partial)) = update_rx.recv().await {
                    yield AgentEvent::ToolExecutionUpdate {
                        tool_use_id,
                        partial,
                    };
                }

                // Append synthetic error results for any tool calls not matched to a
                // registered tool; leaving orphan ToolUse blocks causes a provider 400.
                for (id, name) in unmatched {
                    results.push((
                        id,
                        Err(ToolFailure::new(
                            "unknown_tool",
                            format!("Unknown tool: {name}"),
                        )),
                    ));
                }
                // Trajectory integrity — every ToolUse id must have a paired
                // result. If execute_tool_batch dropped any (abort/timeout/panic
                // recovery), fill synthetic errors so the next provider request
                // doesn't 400.
                let result_ids: Vec<&str> =
                    results.iter().map(|(id, _)| id.as_str()).collect();
                let missing: Vec<String> = tool_calls
                    .iter()
                    .filter(|(id, _, _)| !result_ids.contains(&id.as_str()))
                    .map(|(id, _, _)| id.clone())
                    .collect();
                for id in missing {
                    results.push((
                        id,
                        Err(ToolFailure::new(
                            "missing_result",
                            "Tool execution produced no result for this tool_use_id; \
                             a synthetic error was injected to preserve trajectory integrity.",
                        )),
                    ));
                }

                // Emit ToolExecutionEnd and build ToolResult messages
                for (id, result) in &results {
                    yield AgentEvent::ToolExecutionEnd {
                        tool_use_id: id.clone(),
                        result: result.clone(),
                    };
                    let timestamp = chrono::Utc::now();
                    let model_message = match result {
                        Ok(result) => result.model_message(id.clone(), timestamp),
                        Err(failure) => failure.model_result_message(id.clone(), timestamp),
                    };
                    let session_message = match result {
                        Ok(result) => result.session_message(id.clone(), timestamp),
                        Err(failure) => failure.session_message(id.clone(), timestamp),
                    };
                    let (model_message, session_message) = match (model_message, session_message) {
                        (Ok(model_message), Ok(session_message)) => (model_message, session_message),
                        (Err(error), _) | (_, Err(error)) => {
                            yield AgentEvent::Error(AgentError::Internal(error.to_string()));
                            return;
                        }
                    };
                    ctx.messages.push(AgentMessage::ToolResult(model_message));
                    new_messages.push(AgentMessage::ToolResult(session_message));
                }

                let final_answer_message = current_final_answer_tool
                    .as_ref()
                    .and_then(|_cfg| {
                        final_answer_text_from_results(FINAL_ANSWER_TOOL_NAME, &tool_calls, &results)
                    })
                    .map(|text| {
                        let turn_id = assistant_msg.turn_id.clone();
                        let message_id = Uuid::now_v7().to_string();
                        AssistantMessage {
                            kind: AssistantMessageKind::FinalAnswer,
                            message_id,
                            turn_id,
                            content: vec![ContentBlock::Text { text }],
                            stop_reason: Some(StopReason::EndTurn),
                            timestamp: chrono::Utc::now(),
                            provider: assistant_msg.provider.clone(),
                            api: assistant_msg.api.clone(),
                            model: assistant_msg.model.clone(),
                            usage: None,
                            error_message: None,
                        }
                    });

                if let Some(final_message) = &final_answer_message {
                    if let Err(error) =
                        validate_final_answer(&config, final_message, turn_index).await
                    {
                        yield AgentEvent::Error(error);
                        break 'main;
                    }
                    let message_id = final_message.message_id.clone();
                    yield AgentEvent::MessageStart {
                        message_id: message_id.clone(),
                    };
                    yield AgentEvent::MessageEnd {
                        message_id,
                        message: final_message.clone(),
                        discarded: false,
                    };
                    let final_agent_msg = AgentMessage::Assistant(final_message.clone());
                    ctx.messages.push(final_agent_msg.clone());
                    new_messages.push(final_agent_msg);
                }

                let should_terminate = !results.is_empty()
                    && (final_answer_message.is_some()
                        || results.iter().all(|(_, r)| {
                            r.as_ref().map(|tr| tr.terminate).unwrap_or(false)
                        }));

                // Call PrepareNextTurnHook before consuming results into TurnEnd
                if !should_terminate && let Some(h) = &config.prepare_next_turn {
                    match h.prepare(PrepareNextTurnCtx {
                        run: &config.run,
                        turn_index,
                        last_message: &assistant_msg,
                        last_tool_results: &results,
                    }).await {
                        Ok(directive) => {
                            if let Err(e) = apply_next_turn_directive(
                                &mut ctx,
                                &mut config,
                                &mut active_tools,
                                directive,
                            ) {
                                yield AgentEvent::Error(e);
                                yield AgentEvent::TurnEnd {
                                    index: turn_index,
                                    message: assistant_msg,
                                    tool_results: results,
                                };
                                break 'main;
                            }
                        }
                        Err(e) => {
                            yield AgentEvent::Error(e);
                            yield AgentEvent::TurnEnd {
                                index: turn_index,
                                message: assistant_msg,
                                tool_results: results,
                            };
                            break 'main;
                        }
                    }
                }

                yield AgentEvent::TurnEnd {
                    index: turn_index,
                    message: final_answer_message.unwrap_or(assistant_msg),
                    tool_results: results,
                };

                if should_terminate {
                    break 'main;
                }

                turn_index += 1;
                continue 'main;
            } else {
                // Non-tool turn. Covers two cases:
                //   1. Normal: stop_reason != ToolUse and no ToolUse blocks.
                //   2. Defensive: stop_reason == ToolUse but no ToolUse blocks found
                //      (model claimed tool use but emitted no blocks). Nothing to
                //      repair — log a warning and proceed as non-tool turn.
                if stop_reason == StopReason::ToolUse {
                    tracing::warn!(
                        "stop_reason is ToolUse but no ToolUse blocks found in assistant message; \
                         proceeding as non-tool turn"
                    );
                }

                if let FinalAnswerMode::Tool(cfg) = &config.final_answer_mode
                    && cfg.missing_behavior == FinalAnswerMissingBehavior::Error
                {
                    yield AgentEvent::Error(AgentError::Internal(format!(
                        "final-answer tool '{}' was required but the model stopped without calling it",
                        FINAL_ANSWER_TOOL_NAME
                    )));
                    break 'main;
                }

                // Non-tool stop
                yield AgentEvent::TurnEnd {
                    index: turn_index,
                    message: assistant_msg.clone(),
                    tool_results: vec![],
                };

                let should_stop = if let Some(h) = &config.should_stop {
                    h.should_stop(ShouldStopCtx {
                        run: &config.run,
                        last_assistant: &assistant_msg,
                        stop_reason,
                        turn_index,
                    })
                    .await
                } else {
                    true // default: stop
                };

                if should_stop {
                    // Check follow_up channel
                    if let Some(ref mut rx) = config.follow_up_rx
                        && let Ok(follow_up_msg) = rx.try_recv()
                    {
                        ctx.messages.push(follow_up_msg.clone());
                        new_messages.push(follow_up_msg);
                        turn_index += 1;
                        continue 'main;
                    }
                    break 'main;
                }

                // should_stop = false → call PrepareNextTurnHook and continue
                if let Some(h) = &config.prepare_next_turn {
                    match h.prepare(PrepareNextTurnCtx {
                        run: &config.run,
                        turn_index,
                        last_message: &assistant_msg,
                        last_tool_results: &[],
                    }).await {
                        Ok(directive) => {
                            if let Err(e) = apply_next_turn_directive(
                                &mut ctx,
                                &mut config,
                                &mut active_tools,
                                directive,
                            ) {
                                yield AgentEvent::Error(e);
                                break 'main;
                            }
                        }
                        Err(e) => {
                            yield AgentEvent::Error(e);
                            break 'main;
                        }
                    }
                }

                turn_index += 1;
            }
        }

        yield AgentEvent::AgentEnd { new_messages };
    }
}

#[cfg(test)]
#[cfg(feature = "test-utils")]
mod tests {
    use super::{active_tool_subset, apply_next_turn_directive, llm_error_to_agent_error};

    use crate::test_utils::{MockLlmClient, MockResponse, NoOpEnv};
    use crate::{
        DefaultConvertToLlm, FinalAnswerMode, LoopConfig, agent_loop, agent_loop_continue,
    };
    use futures::StreamExt;
    use futures::future::BoxFuture;
    use llm_adapter::provider::Provider;
    use llm_harness_types::*;
    use std::collections::HashSet;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    struct NamedTool(&'static str);
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            ""
        }
        fn parameters_schema(&self) -> &serde_json::Value {
            static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
            S.get_or_init(|| serde_json::json!({}))
        }
        fn execute<'a>(
            &'a self,
            _: serde_json::Value,
            _: &'a ToolContext,
        ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
            Box::pin(async { Ok(ToolResult::full(vec![], serde_json::Value::Null, false)) })
        }
    }

    struct RejectFinalAnswer;

    impl FinalAnswerValidator for RejectFinalAnswer {
        fn validate<'a>(
            &'a self,
            _: FinalAnswerValidationCtx<'a>,
        ) -> BoxFuture<'a, Result<(), FinalAnswerValidationError>> {
            Box::pin(async {
                Err(FinalAnswerValidationError::new(
                    "test_rejection",
                    "candidate rejected",
                ))
            })
        }
    }

    fn make_config(responses: Vec<MockResponse>) -> (Arc<MockLlmClient>, LoopConfig) {
        let client = Arc::new(MockLlmClient::new(responses));
        let cfg = LoopConfig {
            run: Arc::new(RunContext::new(RunRequest::default())),
            model: "test-model".into(),
            max_tokens: 1024,
            temperature: None,
            thinking_level: ThinkingLevel::Off,
            tools: vec![],
            active_tools: None,
            default_execution_mode: ToolExecutionMode::Parallel,
            final_answer_mode: crate::FinalAnswerMode::default(),
            env: Arc::new(NoOpEnv),
            abort: CancellationToken::new(),
            stream_options: StreamOptions::default(),
            convert_to_llm: Arc::new(DefaultConvertToLlm::new()),
            transform_context: None,
            prepare_next_turn: None,
            should_stop: None,
            before_provider_request: None,
            after_provider_response: None,
            final_answer_validator: None,
            steer_rx: None,
            follow_up_rx: None,
            retry: None,
            response_format: None,
        };
        (client, cfg)
    }

    #[test]
    fn active_tool_subset_filters_without_dropping_registered_tools() {
        let tools: Vec<Arc<dyn Tool>> =
            vec![Arc::new(NamedTool("read")), Arc::new(NamedTool("write"))];
        let active = Some(HashSet::from(["read".to_string()]));

        let subset = active_tool_subset(&tools, &active).unwrap();

        assert_eq!(subset.len(), 1);
        assert_eq!(subset[0].name(), "read");
        assert_eq!(tools.len(), 2, "registered tool set must remain intact");
    }

    #[test]
    fn active_tool_subset_rejects_unknown_tool_names() {
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(NamedTool("read"))];
        let active = Some(HashSet::from(["missing".to_string()]));

        match active_tool_subset(&tools, &active) {
            Err(AgentError::Internal(message)) => assert!(message.contains("missing")),
            Err(err) => panic!("unexpected error: {err:?}"),
            Ok(_) => panic!("unknown active tool should be rejected"),
        }
    }

    #[test]
    fn directive_active_tools_none_restores_all_tools() {
        let mut cfg = make_config(vec![]).1;
        cfg.tools = vec![Arc::new(NamedTool("read")), Arc::new(NamedTool("write"))];
        let mut ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        let mut active = Some(HashSet::from(["read".to_string()]));

        apply_next_turn_directive(
            &mut ctx,
            &mut cfg,
            &mut active,
            NextTurnDirective {
                context: None,
                model: None,
                thinking_level: None,
                temperature: None,
                tools: None,
                active_tools: None,
                response_format: None,
            },
        )
        .unwrap();

        let subset = active_tool_subset(&cfg.tools, &active).unwrap();
        assert!(active.is_none());
        assert_eq!(subset.len(), 2);
    }

    #[test]
    fn directive_temperature_none_preserves_current() {
        let mut cfg = make_config(vec![]).1;
        cfg.temperature = Some(0.7);
        let mut ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        let mut active = None;

        apply_next_turn_directive(
            &mut ctx,
            &mut cfg,
            &mut active,
            NextTurnDirective {
                context: None,
                model: None,
                thinking_level: None,
                temperature: None,
                tools: None,
                active_tools: None,
                response_format: None,
            },
        )
        .unwrap();

        assert_eq!(
            cfg.temperature,
            Some(0.7),
            "None directive must preserve current"
        );
    }

    #[test]
    fn directive_temperature_some_none_clears() {
        let mut cfg = make_config(vec![]).1;
        cfg.temperature = Some(0.7);
        let mut ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        let mut active = None;

        apply_next_turn_directive(
            &mut ctx,
            &mut cfg,
            &mut active,
            NextTurnDirective {
                context: None,
                model: None,
                thinking_level: None,
                temperature: Some(None),
                tools: None,
                active_tools: None,
                response_format: None,
            },
        )
        .unwrap();

        assert_eq!(cfg.temperature, None, "Some(None) must clear temperature");
    }

    #[test]
    fn directive_temperature_some_some_sets_value() {
        let mut cfg = make_config(vec![]).1;
        cfg.temperature = None;
        let mut ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        let mut active = None;

        apply_next_turn_directive(
            &mut ctx,
            &mut cfg,
            &mut active,
            NextTurnDirective {
                context: None,
                model: None,
                thinking_level: None,
                temperature: Some(Some(0.3)),
                tools: None,
                active_tools: None,
                response_format: None,
            },
        )
        .unwrap();

        assert_eq!(cfg.temperature, Some(0.3), "Some(Some(0.3)) must set value");
    }
    #[test]
    fn directive_response_format_some_some_sets_value() {
        let mut cfg = make_config(vec![]).1;
        cfg.response_format = None;
        let mut ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        let mut active = None;

        apply_next_turn_directive(
            &mut ctx,
            &mut cfg,
            &mut active,
            NextTurnDirective {
                context: None,
                model: None,
                thinking_level: None,
                temperature: None,
                tools: None,
                active_tools: None,
                response_format: Some(Some(crate::ResponseFormat::JsonObject)),
            },
        )
        .unwrap();

        assert_eq!(
            cfg.response_format,
            Some(crate::ResponseFormat::JsonObject),
            "Some(Some(JsonObject)) must set response_format"
        );
    }

    #[test]
    fn directive_response_format_some_none_clears_value() {
        let mut cfg = make_config(vec![]).1;
        cfg.response_format = Some(crate::ResponseFormat::JsonObject);
        let mut ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        let mut active = None;

        apply_next_turn_directive(
            &mut ctx,
            &mut cfg,
            &mut active,
            NextTurnDirective {
                context: None,
                model: None,
                thinking_level: None,
                temperature: None,
                tools: None,
                active_tools: None,
                response_format: Some(None),
            },
        )
        .unwrap();

        assert_eq!(
            cfg.response_format, None,
            "Some(None) must clear response_format"
        );
    }

    #[test]
    fn directive_response_format_none_preserves_current() {
        let mut cfg = make_config(vec![]).1;
        cfg.response_format = Some(crate::ResponseFormat::JsonObject);
        let mut ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        let mut active = None;

        apply_next_turn_directive(
            &mut ctx,
            &mut cfg,
            &mut active,
            NextTurnDirective {
                context: None,
                model: None,
                thinking_level: None,
                temperature: None,
                tools: None,
                active_tools: None,
                response_format: None,
            },
        )
        .unwrap();

        assert_eq!(
            cfg.response_format,
            Some(crate::ResponseFormat::JsonObject),
            "None directive must preserve current response_format"
        );
    }

    #[tokio::test]
    async fn simple_text_response_emits_correct_events() {
        let (client, cfg) = make_config(vec![MockResponse::text("Hello!")]);
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::AgentStart)),
            "missing AgentStart"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::MessageStart { .. })),
            "missing MessageStart"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::TextDelta { .. })),
            "missing TextDelta"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::MessageEnd { .. })),
            "missing MessageEnd"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::TurnEnd { .. })),
            "missing TurnEnd"
        );
        assert!(
            events.iter().any(|e| e
                .as_final_answer()
                .map(|(_, _, t)| t == "Hello!")
                .unwrap_or(false)),
            "missing FinalAnswer"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AgentEnd { .. })),
            "missing AgentEnd"
        );
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn first_message_stop_is_the_only_accounted_terminal() {
        use llm_adapter::types::{
            StopReason as AdapterStopReason, StreamEvent as AdapterEvent, Usage as AdapterUsage,
        };
        use std::sync::Mutex;

        struct CaptureUsage(Arc<Mutex<Vec<TokenUsage>>>);

        impl AfterProviderResponseHook for CaptureUsage {
            fn after_response<'a>(
                &'a self,
                ctx: AfterProviderResponseCtx<'a>,
            ) -> BoxFuture<'a, ()> {
                Box::pin(async move {
                    if let Some(usage) = &ctx.info.usage {
                        self.0.lock().unwrap().push(usage.clone());
                    }
                })
            }
        }

        let invalid = AdapterUsage {
            input_tokens: 100,
            cached_input_tokens: 101,
            ..Default::default()
        };
        let valid = AdapterUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        }
        .reported();
        let mut response = MockResponse::text("answer").with_reported_invalid_usage(invalid);
        response.events.push(Ok(AdapterEvent::MessageStop {
            stop_reason: AdapterStopReason::EndTurn,
            usage: valid,
        }));
        let (client, mut config) = make_config(vec![response]);
        let captured = Arc::new(Mutex::new(Vec::new()));
        config.after_provider_response = Some(Arc::new(CaptureUsage(captured.clone())));
        let context = AgentContext {
            system_prompt: None,
            messages: vec![],
        };

        let _: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, context, config)
                .collect()
                .await;

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(
            captured[0].provenance,
            UsageReportProvenance::ReportedInvalid
        );
    }

    #[tokio::test]
    async fn tool_call_then_text_response_two_turns() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct EchoTool {
            called: Arc<AtomicBool>,
        }
        impl Tool for EchoTool {
            fn name(&self) -> &str {
                "echo"
            }
            fn description(&self) -> &str {
                ""
            }
            fn parameters_schema(&self) -> &serde_json::Value {
                static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
                S.get_or_init(|| serde_json::json!({}))
            }
            fn execute<'a>(
                &'a self,
                _: serde_json::Value,
                _: &'a ToolContext,
            ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
                self.called.store(true, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async { Ok(ToolResult::full(vec![], serde_json::Value::Null, false)) })
            }
        }

        let tool_called = Arc::new(AtomicBool::new(false));
        let tool: Arc<dyn Tool> = Arc::new(EchoTool {
            called: tool_called.clone(),
        });

        let (client_raw, mut cfg) = make_config(vec![
            MockResponse::tool_use("c1", "echo", r#"{"x":1}"#),
            MockResponse::text("Done!"),
        ]);
        cfg.tools = vec![tool];
        let client: Arc<dyn Provider> = Arc::clone(&client_raw) as Arc<dyn Provider>;
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };

        let events: Vec<AgentEvent> = agent_loop(client, ctx, cfg).collect().await;

        assert!(tool_called.load(Ordering::SeqCst), "tool was not executed");
        assert_eq!(
            client_raw
                .call_count
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        let tool_exec_ends = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. }))
            .count();
        assert_eq!(tool_exec_ends, 1);
        assert!(
            events.iter().any(|e| e
                .as_final_answer()
                .map(|(_, _, t)| t == "Done!")
                .unwrap_or(false)),
            "final text response should emit FinalAnswer"
        );
    }

    #[tokio::test]
    async fn required_final_answer_tool_emits_synthetic_final_answer() {
        let (client_raw, mut cfg) = make_config(vec![MockResponse::tool_use(
            "answer_1",
            "final_answer",
            r#"{"text":"Done via tool."}"#,
        )]);
        cfg.final_answer_mode = FinalAnswerMode::required_tool();
        cfg.active_tools = Some(HashSet::new());
        let client: Arc<dyn Provider> = Arc::clone(&client_raw) as Arc<dyn Provider>;

        let events: Vec<AgentEvent> = agent_loop(
            client,
            AgentContext {
                system_prompt: None,
                messages: vec![],
            },
            cfg,
        )
        .collect()
        .await;

        let captured = client_raw.captured_requests.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert!(
            captured[0]
                .tools()
                .iter()
                .any(|tool| tool.name == "final_answer"),
            "built-in final_answer tool should be available even when active_tools is empty"
        );
        drop(captured);

        assert!(events.iter().any(|e| {
            e.as_final_answer()
                .map(|(_, _, t)| t == "Done via tool.")
                .unwrap_or(false)
        }));

        let turn_end = events
            .iter()
            .find(|event| matches!(event, AgentEvent::TurnEnd { .. }))
            .expect("missing TurnEnd");
        if let AgentEvent::TurnEnd { message, .. } = turn_end {
            assert_eq!(message.kind, AssistantMessageKind::FinalAnswer);
            assert_eq!(message.text_content(), "Done via tool.");
        }

        let agent_end = events
            .iter()
            .find(|event| matches!(event, AgentEvent::AgentEnd { .. }))
            .expect("missing AgentEnd");
        if let AgentEvent::AgentEnd { new_messages } = agent_end {
            assert!(new_messages.iter().any(|message| matches!(
                message,
                AgentMessage::Assistant(assistant)
                    if assistant.kind == AssistantMessageKind::FinalAnswer
                        && assistant.text_content() == "Done via tool."
            )));
        }
    }

    #[tokio::test]
    async fn rejected_heuristic_final_answer_never_emits_committed_message_end() {
        let (client, mut cfg) = make_config(vec![MockResponse::text("untrusted answer")]);
        cfg.final_answer_validator = Some(Arc::new(RejectFinalAnswer));

        let events: Vec<AgentEvent> = agent_loop(
            Arc::clone(&client) as Arc<dyn Provider>,
            AgentContext {
                system_prompt: None,
                messages: vec![],
            },
            cfg,
        )
        .collect()
        .await;

        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Error(AgentError::FinalAnswerRejected { code, .. })
                if code == "test_rejection"
        )));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::MessageEnd { .. })),
            "a rejected final answer must not cross the committed MessageEnd boundary"
        );
        let new_messages = events.iter().find_map(|event| match event {
            AgentEvent::AgentEnd { new_messages } => Some(new_messages),
            _ => None,
        });
        assert!(new_messages.is_some_and(Vec::is_empty));
    }

    #[tokio::test]
    async fn rejected_synthetic_final_answer_is_not_committed() {
        let (client, mut cfg) = make_config(vec![MockResponse::tool_use(
            "answer_1",
            "final_answer",
            r#"{"text":"untrusted answer"}"#,
        )]);
        cfg.final_answer_mode = FinalAnswerMode::required_tool();
        cfg.final_answer_validator = Some(Arc::new(RejectFinalAnswer));

        let events: Vec<AgentEvent> = agent_loop(
            Arc::clone(&client) as Arc<dyn Provider>,
            AgentContext {
                system_prompt: None,
                messages: vec![],
            },
            cfg,
        )
        .collect()
        .await;

        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Error(AgentError::FinalAnswerRejected { code, .. })
                if code == "test_rejection"
        )));
        assert!(
            !events.iter().any(|event| event.as_final_answer().is_some()),
            "the built-in final_answer tool must use the same validation boundary"
        );
    }

    #[tokio::test]
    async fn required_final_answer_tool_rejects_plain_terminal_text() {
        let (client, mut cfg) = make_config(vec![MockResponse::text("plain answer")]);
        cfg.final_answer_mode = FinalAnswerMode::required_tool();

        let events: Vec<AgentEvent> = agent_loop(
            Arc::clone(&client) as Arc<dyn Provider>,
            AgentContext {
                system_prompt: None,
                messages: vec![],
            },
            cfg,
        )
        .collect()
        .await;

        assert!(events.iter().any(|e| {
            e.as_progress()
                .map(|(_, _, t)| t == "plain answer")
                .unwrap_or(false)
        }));
        assert!(
            !events.iter().any(|e| e.as_final_answer().is_some()),
            "strict final-answer tool contract should not accept plain text as final"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Error(AgentError::Internal(message))
                if message.contains("required") && message.contains("final_answer")
        )));
    }

    #[tokio::test]
    async fn final_answer_tool_fallback_preserves_plain_text_final_answer() {
        let (client, mut cfg) = make_config(vec![MockResponse::text("plain answer")]);
        cfg.final_answer_mode = FinalAnswerMode::tool_with_text_fallback();

        let events: Vec<AgentEvent> = agent_loop(
            Arc::clone(&client) as Arc<dyn Provider>,
            AgentContext {
                system_prompt: None,
                messages: vec![],
            },
            cfg,
        )
        .collect()
        .await;

        assert!(events.iter().any(|e| {
            e.as_final_answer()
                .map(|(_, _, t)| t == "plain answer")
                .unwrap_or(false)
        }));
        assert!(
            !events.iter().any(|event| matches!(
                event,
                AgentEvent::Error(AgentError::Internal(message))
                    if message.contains("final-answer tool")
            )),
            "fallback mode should not error when the model answers with plain text"
        );
    }

    #[tokio::test]
    async fn tool_partial_updates_are_emitted() {
        struct UpdatingTool;
        impl Tool for UpdatingTool {
            fn name(&self) -> &str {
                "update"
            }
            fn description(&self) -> &str {
                ""
            }
            fn parameters_schema(&self) -> &serde_json::Value {
                static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
                S.get_or_init(|| serde_json::json!({}))
            }
            fn execute<'a>(
                &'a self,
                _: serde_json::Value,
                ctx: &'a ToolContext,
            ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
                Box::pin(async move {
                    ctx.update_tx
                        .send(ToolProgress {
                            content: vec![DataBlock::text("partial")],
                            details: serde_json::json!({"step": 1}),
                        })
                        .await
                        .unwrap();

                    Ok(ToolResult::full(
                        vec![DataBlock::text("done")],
                        serde_json::Value::Null,
                        true,
                    ))
                })
            }
        }

        let (client_raw, mut cfg) = make_config(vec![MockResponse::tool_use("c1", "update", "{}")]);
        cfg.tools = vec![Arc::new(UpdatingTool)];
        let client: Arc<dyn Provider> = Arc::clone(&client_raw) as Arc<dyn Provider>;

        let events: Vec<AgentEvent> = agent_loop(
            client,
            AgentContext {
                system_prompt: None,
                messages: vec![],
            },
            cfg,
        )
        .collect()
        .await;

        assert!(events.iter().any(|e| {
            matches!(
                e,
                AgentEvent::ToolExecutionUpdate { tool_use_id, partial }
                    if tool_use_id == "c1"
                        && matches!(&partial.content[0], DataBlock::Text { text, .. } if text == "partial")
            )
        }));
    }

    #[tokio::test]
    async fn tool_with_terminate_true_stops_loop() {
        struct TerminateTool;
        impl Tool for TerminateTool {
            fn name(&self) -> &str {
                "term"
            }
            fn description(&self) -> &str {
                ""
            }
            fn parameters_schema(&self) -> &serde_json::Value {
                static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
                S.get_or_init(|| serde_json::json!({}))
            }
            fn execute<'a>(
                &'a self,
                _: serde_json::Value,
                _: &'a ToolContext,
            ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
                Box::pin(async { Ok(ToolResult::full(vec![], serde_json::Value::Null, true)) })
            }
        }

        let (client_raw, mut cfg) = make_config(vec![MockResponse::tool_use("c1", "term", "{}")]);
        cfg.tools = vec![Arc::new(TerminateTool)];
        let client: Arc<dyn Provider> = Arc::clone(&client_raw) as Arc<dyn Provider>;

        let events: Vec<AgentEvent> = agent_loop(
            client,
            AgentContext {
                system_prompt: None,
                messages: vec![],
            },
            cfg,
        )
        .collect()
        .await;

        assert_eq!(
            client_raw
                .call_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AgentEnd { .. }))
        );
    }

    #[tokio::test]
    async fn abort_signal_stops_loop() {
        let (_, cfg) = make_config(vec![MockResponse::text("hello")]);
        let abort = cfg.abort.clone();
        abort.cancel();

        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        let events: Vec<AgentEvent> = agent_loop(
            Arc::new(MockLlmClient::new(vec![])) as Arc<dyn Provider>,
            ctx,
            cfg,
        )
        .collect()
        .await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Error(AgentError::Aborted)))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AgentEnd { .. }))
        );
    }

    #[tokio::test]
    async fn agent_end_carries_new_messages() {
        let (client, cfg) = make_config(vec![MockResponse::text("Hi!")]);
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        let agent_end = events
            .iter()
            .find(|e| matches!(e, AgentEvent::AgentEnd { .. }))
            .unwrap();
        if let AgentEvent::AgentEnd { new_messages } = agent_end {
            assert!(!new_messages.is_empty());
            assert!(
                new_messages
                    .iter()
                    .any(|m| matches!(m, AgentMessage::Assistant(_)))
            );
        }
    }

    #[tokio::test]
    async fn agent_loop_continue_drains_stale_steer() {
        use tokio::sync::mpsc;
        let (steer_tx, steer_rx) = mpsc::channel(16);
        let stale_msg = AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "stale".into(),
            }],
            timestamp: chrono::Utc::now(),
        });
        steer_tx.send(stale_msg).await.unwrap();

        let (client, mut cfg) = make_config(vec![MockResponse::text("response")]);
        cfg.steer_rx = Some(steer_rx);

        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        let events: Vec<AgentEvent> =
            agent_loop_continue(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        let agent_end = events
            .iter()
            .find(|e| matches!(e, AgentEvent::AgentEnd { .. }))
            .unwrap();
        if let AgentEvent::AgentEnd { new_messages } = agent_end {
            let has_stale = new_messages.iter().any(|m| {
                if let AgentMessage::User(u) = m {
                    u.content
                        .iter()
                        .any(|c| matches!(c, ContentBlock::Text { text } if text == "stale"))
                } else {
                    false
                }
            });
            assert!(!has_stale, "stale steer message should have been discarded");
        }
    }

    #[tokio::test]
    async fn agent_loop_continue_emits_agent_start() {
        let (client, cfg) = make_config(vec![MockResponse::text("hi")]);
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        let events: Vec<AgentEvent> =
            agent_loop_continue(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::AgentStart)),
            "missing AgentStart"
        );
    }

    #[tokio::test]
    async fn retry_attempt_event_emitted_before_sleep() {
        use crate::config::RetryConfig;

        // Two rate-limit errors then a successful text response.
        let (client, mut cfg) = make_config(vec![
            MockResponse::rate_limit_error(),
            MockResponse::rate_limit_error(),
            MockResponse::text("ok"),
        ]);
        cfg.retry = Some(RetryConfig::new(3, 0)); // base_delay_ms=0 → no real wait
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text { text: "hi".into() }],
                timestamp: chrono::Utc::now(),
            })],
        };
        tokio::time::pause(); // make sleep instant
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        let retry_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::RetryAttempt { .. }))
            .collect();
        assert_eq!(retry_events.len(), 2, "expected two RetryAttempt events");

        // First retry: attempt=0, max_retries=3
        match &retry_events[0] {
            AgentEvent::RetryAttempt {
                attempt,
                max_retries,
                delay_ms: _,
                error,
            } => {
                assert_eq!(*attempt, 0);
                assert_eq!(*max_retries, 3);
                assert!(!error.is_empty());
            }
            _ => unreachable!(),
        }

        // Second retry: attempt=1
        match &retry_events[1] {
            AgentEvent::RetryAttempt { attempt, .. } => assert_eq!(*attempt, 1),
            _ => unreachable!(),
        }

        // After retries, loop must succeed: AgentEnd must be present
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AgentEnd { .. })),
            "loop should complete after successful retry"
        );
    }

    #[tokio::test]
    async fn retry_attempt_event_precedes_message_start() {
        use crate::config::RetryConfig;

        // One error then success: RetryAttempt must come before MessageStart.
        let (client, mut cfg) = make_config(vec![
            MockResponse::rate_limit_error(),
            MockResponse::text("ok"),
        ]);
        cfg.retry = Some(RetryConfig::new(2, 0));
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text { text: "hi".into() }],
                timestamp: chrono::Utc::now(),
            })],
        };
        tokio::time::pause();
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        let retry_pos = events
            .iter()
            .position(|e| matches!(e, AgentEvent::RetryAttempt { .. }))
            .expect("RetryAttempt must appear");
        let msg_start_pos = events
            .iter()
            .position(|e| matches!(e, AgentEvent::MessageStart { .. }))
            .expect("MessageStart must appear");
        assert!(
            retry_pos < msg_start_pos,
            "RetryAttempt (pos {retry_pos}) must precede MessageStart (pos {msg_start_pos})"
        );
    }

    #[tokio::test]
    async fn non_retryable_error_does_not_emit_retry_attempt() {
        use crate::config::RetryConfig;
        // InvalidRequest is not retryable → no RetryAttempt, loop emits Error.
        // `Provider` is already imported at the top of the test module.
        use llm_adapter::LlmError;

        struct AlwaysInvalidClient;

        #[async_trait::async_trait]
        impl llm_adapter::provider::Provider for AlwaysInvalidClient {
            fn capabilities(&self) -> llm_adapter::provider::ProviderCapabilities {
                llm_adapter::provider::ProviderCapabilities::new(false, false, false)
            }
            async fn chat(
                &self,
                _: &llm_adapter::types::ChatRequest,
            ) -> Result<llm_adapter::types::ChatResponse, LlmError> {
                Err(LlmError::InvalidRequest("bad".into()))
            }
            async fn chat_stream(
                &self,
                _: &llm_adapter::types::ChatRequest,
            ) -> Result<llm_adapter::stream_handle::StreamHandle, LlmError> {
                Err(LlmError::InvalidRequest("bad".into()))
            }
        }

        let (_, mut cfg) = make_config(vec![]);
        cfg.retry = Some(RetryConfig::new(3, 0));
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text { text: "hi".into() }],
                timestamp: chrono::Utc::now(),
            })],
        };
        let events: Vec<AgentEvent> = agent_loop(Arc::new(AlwaysInvalidClient), ctx, cfg)
            .collect()
            .await;

        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::RetryAttempt { .. })),
            "non-retryable error must not emit RetryAttempt"
        );
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Error(_))),
            "non-retryable error must emit Error event"
        );
    }
    #[tokio::test]
    async fn abort_during_retry_sleep_stops_loop() {
        use crate::config::RetryConfig;

        // A rate-limit error triggers retry backoff; abort during the sleep
        // must interrupt the loop with Error(Aborted), not wait for backoff.
        let (client, mut cfg) = make_config(vec![
            MockResponse::rate_limit_error(),
            MockResponse::text("ok"), // never reached
        ]);
        cfg.retry = Some(RetryConfig::new(3, 10_000)); // 10s backoff
        let abort = cfg.abort.clone();

        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text { text: "hi".into() }],
                timestamp: chrono::Utc::now(),
            })],
        };

        tokio::time::pause();

        let stream = agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg);
        futures::pin_mut!(stream);
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            let is_retry = matches!(ev, AgentEvent::RetryAttempt { .. });
            events.push(ev);
            if is_retry {
                abort.cancel();
            }
        }

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Error(AgentError::Aborted))),
            "expected Error(Aborted) after abort during retry sleep"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::MessageStart { .. })),
            "no MessageStart should appear after abort during retry sleep"
        );
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "provider should be called exactly once before abort"
        );
    }

    #[tokio::test]
    async fn system_prompt_prepended_to_provider_messages() {
        let (client, cfg) = make_config(vec![MockResponse::text("hi")]);
        let ctx = AgentContext {
            system_prompt: Some("You are a test assistant.".into()),
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
                timestamp: chrono::Utc::now(),
            })],
        };
        let _events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        let captured = client.captured_requests.lock().unwrap();
        assert_eq!(captured.len(), 1, "provider should be called exactly once");
        let messages = captured[0].messages();
        assert!(
            matches!(
                messages.first(),
                Some(llm_adapter::types::Message::System(s)) if s == "You are a test assistant."
            ),
            "system prompt must be the first message sent to the provider"
        );
    }
    #[tokio::test]
    async fn stream_idle_timeout_triggers_retry() {
        use crate::config::RetryConfig;

        // First response: stall after 1 event (stream stalls).
        // Second response: normal text response (retry succeeds).
        let (client, mut cfg) = make_config(vec![
            MockResponse::stall_after(1),
            MockResponse::text("recovered"),
        ]);
        cfg.retry = Some(RetryConfig::new(2, 0)); // base_delay_ms=0
        cfg.stream_options.stream_idle_timeout_ms = Some(100);

        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text { text: "hi".into() }],
                timestamp: chrono::Utc::now(),
            })],
        };

        tokio::time::pause();
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        // Must have a RetryAttempt from the idle timeout
        let retry_count = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::RetryAttempt { .. }))
            .count();
        assert_eq!(
            retry_count, 1,
            "expected one RetryAttempt from idle timeout"
        );

        // Must complete successfully after retry
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AgentEnd { .. })),
            "loop should complete after successful retry"
        );

        // Verify the RetryAttempt was triggered by an idle timeout (typed
        // variant check — not a string substring match on the Display text).
        // A RetryAttempt is only emitted on the idle-retry path (is_idle &&
        // can_retry), so its presence after a stall confirms the typed
        // dispatch contract.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::RetryAttempt { .. })),
            "RetryAttempt should be present, confirming idle-timeout typed dispatch"
        );
    }

    #[tokio::test]
    async fn stream_idle_timeout_exhausts_retries() {
        use crate::config::RetryConfig;

        // All responses stall — retries exhaust, loop emits Error.
        let (client, mut cfg) = make_config(vec![
            MockResponse::stall_after(1),
            MockResponse::stall_after(1),
            MockResponse::stall_after(1),
        ]);
        cfg.retry = Some(RetryConfig::new(2, 0)); // max_retries=2
        cfg.stream_options.stream_idle_timeout_ms = Some(50);

        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text { text: "hi".into() }],
                timestamp: chrono::Utc::now(),
            })],
        };

        tokio::time::pause();
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        let retry_count = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::RetryAttempt { .. }))
            .count();
        assert_eq!(
            retry_count, 2,
            "expected 2 RetryAttempt events (max_retries=2)"
        );

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Error(AgentError::StreamIdle { .. }))),
            "expected Error(StreamIdle) after retries exhausted"
        );
    }

    #[tokio::test]
    async fn stream_idle_timeout_disabled_by_default() {
        use crate::config::RetryConfig;

        // stream_idle_timeout_ms=None → watchdog disabled, no RetryAttempt emitted.
        let (client, mut cfg) = make_config(vec![MockResponse::stall_after(1)]);
        cfg.retry = Some(RetryConfig::new(3, 0));

        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text { text: "hi".into() }],
                timestamp: chrono::Utc::now(),
            })],
        };

        tokio::time::pause();

        let stream = agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg);
        futures::pin_mut!(stream);

        let mut events = Vec::new();
        let deadline = tokio::time::sleep(std::time::Duration::from_millis(500));
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                _ = &mut deadline => break,
                ev = stream.next() => {
                    match ev {
                        Some(e) => events.push(e),
                        None => break,
                    }
                }
            }
        }

        // No RetryAttempt should have been emitted (watchdog disabled)
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::RetryAttempt { .. })),
            "no RetryAttempt when stream_idle_timeout_ms is None"
        );
    }

    #[tokio::test]
    async fn idle_timer_resets_on_event() {
        use crate::config::RetryConfig;

        // slow_stream emits events at 10ms intervals; idle timeout=50ms.
        // Each event resets the timer, so watchdog should NOT fire.
        let events = MockResponse::text("slow").events;
        let (client, mut cfg) = make_config(vec![MockResponse::slow_stream(events, 10)]);
        cfg.retry = Some(RetryConfig::new(3, 0));
        cfg.stream_options.stream_idle_timeout_ms = Some(50);

        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text { text: "hi".into() }],
                timestamp: chrono::Utc::now(),
            })],
        };

        tokio::time::pause();
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        // Should complete normally — no RetryAttempt
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::RetryAttempt { .. })),
            "no RetryAttempt when events arrive within idle window"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AgentEnd { .. })),
            "loop should complete normally"
        );
    }

    #[tokio::test]
    async fn abort_during_idle_wait() {
        use crate::config::RetryConfig;

        let (client, mut cfg) = make_config(vec![MockResponse::stall_after(1)]);
        cfg.retry = Some(RetryConfig::new(3, 0));
        cfg.stream_options.stream_idle_timeout_ms = Some(10_000); // 10s idle
        let abort = cfg.abort.clone();

        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text { text: "hi".into() }],
                timestamp: chrono::Utc::now(),
            })],
        };

        tokio::time::pause();

        let stream = agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg);
        futures::pin_mut!(stream);
        let mut events = Vec::new();

        while let Some(ev) = stream.next().await {
            // Abort after MessageStart (stream is now stalled waiting for events)
            let is_error = matches!(ev, AgentEvent::Error(_));
            if matches!(ev, AgentEvent::MessageStart { .. }) {
                abort.cancel();
            }
            events.push(ev);
            if is_error {
                break;
            }
        }

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Error(AgentError::Aborted))),
            "expected Error(Aborted) when abort fires during idle wait"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::RetryAttempt { .. })),
            "no RetryAttempt — abort should take priority over idle timeout"
        );
    }

    #[tokio::test]
    async fn retry_after_idle_timeout_succeeds() {
        use crate::config::RetryConfig;

        // First: stall. Second: normal response. Retry should succeed.
        let (client, mut cfg) = make_config(vec![
            MockResponse::stall_after(1),
            MockResponse::text("ok after retry"),
        ]);
        cfg.retry = Some(RetryConfig::new(2, 0));
        cfg.stream_options.stream_idle_timeout_ms = Some(100);

        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text { text: "hi".into() }],
                timestamp: chrono::Utc::now(),
            })],
        };

        tokio::time::pause();
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        // Should have exactly 1 RetryAttempt
        let retry_count = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::RetryAttempt { .. }))
            .count();
        assert_eq!(retry_count, 1);

        // Should complete with AgentEnd
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AgentEnd { .. })),
            "loop should complete after successful retry"
        );

        // new_messages should contain the assistant message
        if let Some(AgentEvent::AgentEnd { new_messages }) = events
            .iter()
            .rev()
            .find(|e| matches!(e, AgentEvent::AgentEnd { .. }))
        {
            assert!(
                !new_messages.is_empty(),
                "should have new messages after successful retry"
            );
        }
    }

    #[tokio::test]
    async fn idle_retry_emits_discard_message_end_for_partial_message() {
        use crate::config::RetryConfig;

        // stall_after(2) emits ContentStart + TextDelta (a visible partial),
        // then stalls. On idle timeout the loop must emit a discard MessageEnd
        // for the orphaned partial message before retrying with a fresh
        // MessageStart.
        let (client, mut cfg) = make_config(vec![
            MockResponse::stall_after(2),
            MockResponse::text("recovered"),
        ]);
        cfg.retry = Some(RetryConfig::new(2, 0));
        cfg.stream_options.stream_idle_timeout_ms = Some(100);

        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text { text: "hi".into() }],
                timestamp: chrono::Utc::now(),
            })],
        };

        tokio::time::pause();
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        // Collect message_ids seen in MessageStart order.
        let starts: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::MessageStart { message_id } => Some(message_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            starts.len(),
            2,
            "expected two MessageStart events (stall + retry)"
        );

        // The first (stalled) message_id must have a matching MessageEnd
        // emitted BEFORE the second MessageStart — the discard signal.
        let stalled_id = &starts[0];
        let retry_id = &starts[1];
        assert_ne!(stalled_id, retry_id, "retry must start a fresh message_id");

        let stalled_end_idx = events.iter().position(|e| {
            matches!(
                e,
                AgentEvent::MessageEnd { message_id, .. } if message_id == stalled_id
            )
        });
        let retry_start_idx = events.iter().position(|e| {
            matches!(
                e,
                AgentEvent::MessageStart { message_id } if message_id == retry_id
            )
        });
        assert!(
            stalled_end_idx.is_some(),
            "partial message must be closed with MessageEnd"
        );
        assert!(
            retry_start_idx.is_some(),
            "retry must emit a fresh MessageStart"
        );
        assert!(
            stalled_end_idx.unwrap() < retry_start_idx.unwrap(),
            "discard MessageEnd for partial message must precede the retry's MessageStart"
        );

        // The discard MessageEnd must carry a Progress (non-final) message,
        // be marked discarded, and have cleared content.
        if let Some(AgentEvent::MessageEnd {
            message, discarded, ..
        }) = events.iter().find(|e| {
            matches!(
                e,
                AgentEvent::MessageEnd { message_id, .. } if message_id == stalled_id
            )
        }) {
            assert_eq!(
                message.kind,
                AssistantMessageKind::Progress,
                "discard MessageEnd should be a Progress partial, not FinalAnswer"
            );
            assert!(*discarded, "discard MessageEnd must have discarded: true");
            assert!(
                message.content.is_empty(),
                "discard MessageEnd content must be cleared"
            );
        }

        // Loop completes after successful retry.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AgentEnd { .. })),
            "loop should complete after successful retry"
        );
    }

    #[tokio::test]
    async fn idle_retry_stall_after_one_event_emits_discard_message_end() {
        use crate::config::RetryConfig;

        // stall_after(1) emits only ContentStart (which produces no AgentEvent
        // from state.process), then stalls. MessageStart was already emitted
        // unconditionally, so the idle retry must still close it with a discard
        // MessageEnd — even though no visible token-level event followed.
        let (client, mut cfg) = make_config(vec![
            MockResponse::stall_after(1),
            MockResponse::text("recovered"),
        ]);
        cfg.retry = Some(RetryConfig::new(2, 0));
        cfg.stream_options.stream_idle_timeout_ms = Some(100);

        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text { text: "hi".into() }],
                timestamp: chrono::Utc::now(),
            })],
        };

        tokio::time::pause();
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        let starts: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::MessageStart { message_id } => Some(message_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            starts.len(),
            2,
            "expected two MessageStart events (stall + retry)"
        );

        let stalled_id = &starts[0];

        // The stalled MessageStart must be closed by a discard MessageEnd
        // before the retry's MessageStart.
        let stalled_end_idx = events.iter().position(|e| matches!(
            e,
            AgentEvent::MessageEnd { message_id, discarded: true, .. } if message_id == stalled_id
        ));
        let retry_start_idx = events.iter().position(|e| {
            matches!(
                e,
                AgentEvent::MessageStart { message_id } if message_id == &starts[1]
            )
        });
        assert!(
            stalled_end_idx.is_some(),
            "stalled MessageStart must be closed with a discard MessageEnd even after ContentStart-only stall"
        );
        assert!(retry_start_idx.is_some());
        assert!(
            stalled_end_idx.unwrap() < retry_start_idx.unwrap(),
            "discard MessageEnd must precede the retry's MessageStart"
        );

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AgentEnd { .. })),
            "loop should complete after successful retry"
        );
    }

    #[tokio::test]
    async fn idle_timeout_with_aborted_retry_sleep() {
        use crate::config::RetryConfig;

        // Stall → idle timeout → retry sleep is long → abort during sleep.
        let (client, mut cfg) = make_config(vec![
            MockResponse::stall_after(1),
            MockResponse::text("never reached"),
        ]);
        cfg.retry = Some(RetryConfig::new(2, 10_000)); // 10s backoff
        cfg.stream_options.stream_idle_timeout_ms = Some(50);
        let abort = cfg.abort.clone();

        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text { text: "hi".into() }],
                timestamp: chrono::Utc::now(),
            })],
        };

        tokio::time::pause();

        let stream = agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg);
        futures::pin_mut!(stream);
        let mut events = Vec::new();

        while let Some(ev) = stream.next().await {
            let is_retry = matches!(ev, AgentEvent::RetryAttempt { .. });
            let is_error = matches!(ev, AgentEvent::Error(_));
            events.push(ev);
            if is_retry {
                abort.cancel();
            }
            if is_error {
                break;
            }
        }

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Error(AgentError::Aborted))),
            "expected Error(Aborted) after abort during retry sleep"
        );
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "provider should be called once before abort during retry sleep"
        );
    }

    #[tokio::test]
    async fn response_format_threaded_to_provider_request() {
        let (client, mut cfg) = make_config(vec![MockResponse::text("hi")]);
        cfg.response_format = Some(crate::ResponseFormat::JsonObject);
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
                timestamp: chrono::Utc::now(),
            })],
        };
        let _events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        let captured = client.captured_requests.lock().unwrap();
        assert_eq!(captured.len(), 1, "provider should be called exactly once");
        assert_eq!(
            captured[0].response_format(),
            Some(&crate::ResponseFormat::JsonObject),
            "response_format must be threaded to the ChatRequest sent to the provider"
        );
    }

    #[tokio::test]
    async fn tool_use_block_with_end_turn_stop_reason_gets_synthetic_result() {
        // Model emits a ToolUse block for an unregistered tool but with stop_reason=EndTurn.
        // Without repair, this ToolUse would be orphaned → 400 on next turn.
        let (client_raw, cfg) = make_config(vec![
            MockResponse::tool_use_end_turn("tu_01", "nonexistent_tool", r#"{}"#),
            MockResponse::text("Recovered."),
        ]);
        let client: Arc<dyn Provider> = Arc::clone(&client_raw) as Arc<dyn Provider>;
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };

        let events: Vec<AgentEvent> = agent_loop(client, ctx, cfg).collect().await;

        // The orphan ToolUse must have produced a synthetic ToolExecutionEnd.
        let tool_exec_ends: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. }))
            .collect();
        assert_eq!(
            tool_exec_ends.len(),
            1,
            "orphan ToolUse must produce a synthetic ToolExecutionEnd"
        );

        // The synthetic result must be an error (unknown_tool or missing_result).
        if let AgentEvent::ToolExecutionEnd { result, .. } = &tool_exec_ends[0] {
            assert!(
                result.is_err(),
                "synthetic result for orphan ToolUse must be Err"
            );
        }

        // Loop must have continued to the second turn (no 400, no crash).
        assert_eq!(
            client_raw
                .call_count
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "loop must continue to second turn after repair"
        );

        // Trajectory invariant: every ToolUse id in the second request's history
        // must have a paired ToolResult. This directly verifies the core goal
        // of the design — no orphan ToolUse reaches the provider.
        let captured = client_raw.captured_requests.lock().unwrap();
        assert_eq!(captured.len(), 2, "two provider calls expected");
        use llm_adapter::types::{Message, ResponseContent};
        let second_request_msgs = captured[1].messages();
        let mut tool_use_ids: Vec<&str> = vec![];
        let mut tool_result_ids: Vec<&str> = vec![];
        for msg in second_request_msgs {
            match msg {
                Message::Assistant(content) => {
                    for block in content {
                        if let ResponseContent::ToolInvocation(ti) = block {
                            tool_use_ids.push(ti.id.as_str());
                        }
                    }
                }
                Message::Tool { invocation_id, .. } => {
                    tool_result_ids.push(invocation_id.as_str());
                }
                _ => {}
            }
        }
        for id in &tool_use_ids {
            assert!(
                tool_result_ids.contains(id),
                "ToolUse id '{id}' has no paired ToolResult in the second request — trajectory integrity violated"
            );
        }
    }

    #[tokio::test]
    async fn registered_tool_with_end_turn_stop_reason_still_executes() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct EchoTool {
            called: Arc<AtomicBool>,
        }
        impl Tool for EchoTool {
            fn name(&self) -> &str {
                "echo"
            }
            fn description(&self) -> &str {
                ""
            }
            fn parameters_schema(&self) -> &serde_json::Value {
                static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
                S.get_or_init(|| serde_json::json!({}))
            }
            fn execute<'a>(
                &'a self,
                _: serde_json::Value,
                _: &'a ToolContext,
            ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
                self.called.store(true, Ordering::SeqCst);
                Box::pin(async { Ok(ToolResult::full(vec![], serde_json::Value::Null, false)) })
            }
        }

        let tool_called = Arc::new(AtomicBool::new(false));
        let tool: Arc<dyn Tool> = Arc::new(EchoTool {
            called: tool_called.clone(),
        });

        let (client_raw, mut cfg) = make_config(vec![
            MockResponse::tool_use_end_turn("tu_01", "echo", r#"{"x":1}"#),
            MockResponse::text("Done."),
        ]);
        cfg.tools = vec![tool];
        let client: Arc<dyn Provider> = Arc::clone(&client_raw) as Arc<dyn Provider>;
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };

        let events: Vec<AgentEvent> = agent_loop(client, ctx, cfg).collect().await;

        assert!(
            tool_called.load(Ordering::SeqCst),
            "registered tool must execute even with EndTurn stop_reason"
        );
        assert_eq!(
            client_raw
                .call_count
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "loop must continue after tool execution"
        );

        // Verify the tool produced a ToolExecutionEnd event.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. })),
            "registered tool must emit ToolExecutionEnd"
        );

        // Trajectory invariant: the ToolUse id must have a paired ToolResult
        // in the second request's history — no orphan ToolUse reaches the provider.
        let captured = client_raw.captured_requests.lock().unwrap();
        assert_eq!(captured.len(), 2, "two provider calls expected");
        use llm_adapter::types::{Message, ResponseContent};
        let second_request_msgs = captured[1].messages();
        let mut tool_use_ids: Vec<&str> = vec![];
        let mut tool_result_ids: Vec<&str> = vec![];
        for msg in second_request_msgs {
            match msg {
                Message::Assistant(content) => {
                    for block in content {
                        if let ResponseContent::ToolInvocation(ti) = block {
                            tool_use_ids.push(ti.id.as_str());
                        }
                    }
                }
                Message::Tool { invocation_id, .. } => {
                    tool_result_ids.push(invocation_id.as_str());
                }
                _ => {}
            }
        }
        for id in &tool_use_ids {
            assert!(
                tool_result_ids.contains(id),
                "ToolUse id '{id}' has no paired ToolResult in the second request — trajectory integrity violated"
            );
        }
        // The mismatched provider stop reason must not commit the tool turn as
        // FinalAnswer. Consumers rely on FinalAnswer being the last assistant
        // message boundary in a run.
        let completed_messages: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::MessageEnd {
                    message,
                    discarded: false,
                    ..
                } => Some(message),
                _ => None,
            })
            .collect();
        assert_eq!(
            completed_messages.len(),
            2,
            "tool progress and final answer must each close one message"
        );
        assert_eq!(
            completed_messages[0].kind,
            AssistantMessageKind::Progress,
            "a message containing ToolUse cannot be committed as FinalAnswer"
        );
        assert_eq!(
            completed_messages[1].kind,
            AssistantMessageKind::FinalAnswer,
            "the post-tool response must remain the only final answer"
        );
    }

    #[tokio::test]
    async fn tool_use_stop_reason_without_blocks_proceeds_as_non_tool_turn() {
        // Model claims stop_reason=ToolUse but emits no ToolUse blocks.
        // Defensive path: no orphan to repair, proceed as non-tool turn.
        use llm_adapter::types::StopReason as AdapterStop;

        let (client_raw, cfg) = make_config(vec![
            MockResponse::text("Hello!").with_stop_reason(AdapterStop::ToolUse),
            MockResponse::text("Done."),
        ]);
        let client: Arc<dyn Provider> = Arc::clone(&client_raw) as Arc<dyn Provider>;
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };

        let events: Vec<AgentEvent> = agent_loop(client, ctx, cfg).collect().await;

        // No ToolExecutionEnd should be emitted (no tool calls).
        let tool_exec_ends = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. }))
            .count();
        assert_eq!(
            tool_exec_ends, 0,
            "no ToolExecutionEnd for empty tool_calls"
        );
    }

    #[test]
    fn llm_error_to_agent_error_maps_every_variant_to_typed_kind() {
        use llm_adapter::LlmError as E;

        // LlmError::Network(reqwest::Error) is not constructible in a unit
        // test; its mapping (-> ProviderErrorKind::Network) is covered by the
        // exhaustive compiler check on llm_error_to_agent_error itself.
        type KindPredicate = fn(&ProviderErrorKind) -> bool;
        let cases: Vec<(E, KindPredicate)> = vec![
            (E::InvalidRequest("bad".into()), |k| {
                matches!(k, ProviderErrorKind::InvalidRequest(_))
            }),
            (E::Unauthorized, |k| {
                matches!(k, ProviderErrorKind::Unauthorized)
            }),
            (E::Forbidden, |k| matches!(k, ProviderErrorKind::Forbidden)),
            (E::RateLimit { retry_after: None }, |k| {
                matches!(k, ProviderErrorKind::RateLimit { retry_after: None })
            }),
            (
                E::RateLimit {
                    retry_after: Some(std::time::Duration::from_secs(5)),
                },
                |k| matches!(k, ProviderErrorKind::RateLimit { retry_after: Some(d) } if *d == std::time::Duration::from_secs(5)),
            ),
            (E::Overloaded { retry_after: None }, |k| {
                matches!(k, ProviderErrorKind::Overloaded { retry_after: None })
            }),
            (E::ServerError("boom".into()), |k| {
                matches!(k, ProviderErrorKind::ServerError(_))
            }),
            (E::Timeout, |k| matches!(k, ProviderErrorKind::Timeout)),
            (E::Stream("cut".into()), |k| {
                matches!(k, ProviderErrorKind::Stream(_))
            }),
            (
                E::StreamIncomplete {
                    received_chunks: 3,
                    finish_reason: None,
                },
                |k| {
                    matches!(
                        k,
                        ProviderErrorKind::StreamIncomplete {
                            received_chunks: 3,
                            finish_reason: None
                        }
                    )
                },
            ),
            (
                E::StreamIncomplete {
                    received_chunks: 5,
                    finish_reason: Some("length".into()),
                },
                |k| matches!(k, ProviderErrorKind::StreamIncomplete { received_chunks: 5, finish_reason: Some(fr) } if fr == "length"),
            ),
            (
                E::Decode(serde_json::from_str::<i32>("not-an-int").unwrap_err()),
                |k| matches!(k, ProviderErrorKind::Decode(_)),
            ),
            (
                E::Provider {
                    code: "E429".into(),
                    message: "x".into(),
                },
                |k| matches!(k, ProviderErrorKind::Other { code } if code == "E429"),
            ),
        ];

        for (llm_err, expect_kind) in cases {
            let err = llm_error_to_agent_error(llm_err);
            let AgentError::ProviderTyped { message, kind } = &err else {
                panic!("expected ProviderTyped, got {err:?}");
            };
            assert!(
                expect_kind(kind),
                "unexpected kind {kind:?} for {message:?}"
            );
            // Display must surface the human-readable message (issue: it was dropped).
            assert!(!message.is_empty());
            assert!(err.to_string().contains(message));
        }
    }
}
