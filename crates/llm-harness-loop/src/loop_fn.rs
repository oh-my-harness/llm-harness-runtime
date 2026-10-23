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
            kind,
            detail,
        } => ProviderErrorKind::StreamIncomplete {
            received_chunks: *received_chunks,
            finish_reason: finish_reason.clone(),
            kind: *kind,
            detail: detail.clone(),
        },
        LlmError::Decode(_) => ProviderErrorKind::Decode(e.to_string()),
        LlmError::Provider { code, .. } => ProviderErrorKind::Other { code: code.clone() },
    };
    AgentError::ProviderTyped {
        message: e.to_string(),
        kind,
    }
}

/// Stream-consumption failures that are safe to retry at request level.
///
/// Only a generation that never reached `MessageStop` qualifies: its tool
/// calls are not executed, so resending the request cannot duplicate a side
/// effect. Admission failures keep their existing policy.
fn is_retryable_stream(error: &AgentError) -> bool {
    matches!(
        error,
        AgentError::ProviderTyped {
            kind: ProviderErrorKind::StreamIncomplete { .. },
            ..
        }
    )
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
#[cfg(not(target_arch = "wasm32"))]
pub fn agent_loop(
    client: Arc<dyn Provider>,
    ctx: AgentContext,
    config: LoopConfig,
) -> impl Stream<Item = AgentEvent> + Send {
    run_loop(client, ctx, config, false)
}

/// wasm32: provider futures are `!Send` (JS promises under reqwest's wasm
/// backend), so the generated stream cannot carry a `Send` bound. Consume
/// it from the JS task that created it.
#[cfg(target_arch = "wasm32")]
pub fn agent_loop(
    client: Arc<dyn Provider>,
    ctx: AgentContext,
    config: LoopConfig,
) -> impl Stream<Item = AgentEvent> {
    run_loop(client, ctx, config, false)
}

/// Continue an agent loop without injecting new initial messages.
///
/// Drains stale steer messages (already queued before this call) before the first LLM call.
#[cfg(not(target_arch = "wasm32"))]
pub fn agent_loop_continue(
    client: Arc<dyn Provider>,
    ctx: AgentContext,
    config: LoopConfig,
) -> impl Stream<Item = AgentEvent> + Send {
    run_loop(client, ctx, config, true)
}

/// Continue an agent loop without injecting new initial messages.
///
/// Drains stale steer messages (already queued before this call) before the first LLM call.
#[cfg(target_arch = "wasm32")]
pub fn agent_loop_continue(
    client: Arc<dyn Provider>,
    ctx: AgentContext,
    config: LoopConfig,
) -> impl Stream<Item = AgentEvent> {
    run_loop(client, ctx, config, true)
}

fn run_loop(
    client: Arc<dyn Provider>,
    mut ctx: AgentContext,
    mut config: LoopConfig,
    drain_steer_first: bool,
) -> impl Stream<Item = AgentEvent> {
    async_stream::stream! {
        use crate::{
            dispatch::execute_tool_batch,
            stream_state::StreamingState,
            type_bridge::tools_to_defs,
        };
        use llm_adapter::types::{ChatRequest, Message as AMsg, ToolChoice};
        use tokio::sync::mpsc;

        // Drain stale steer messages for agent_loop_continue
        if drain_steer_first && let Some(ref mut rx) = config.steer_rx {
            while rx.try_recv().is_ok() {}
        }

        yield AgentEvent::AgentStart;

        let mut turn_index: u32 = 0;
        let mut new_messages: Vec<AgentMessage> = vec![];
        let mut active_tools = config.active_tools.clone();
        // Provider-error-hook repair resends for the CURRENT turn. The hook
        // may only resend a request a bounded number of times: the adapter
        // folds some outcome-unknown failures (e.g. HTTP 504 from a gateway)
        // into `InvalidRequest`, so the loop cannot tell a trusted input
        // rejection from an unknown-outcome failure — an unbounded hook
        // retry would replay the request forever. Reset per turn.
        let max_provider_error_repairs = config.max_provider_error_retries.unwrap_or(1);
        let mut provider_error_repairs: u32 = 0;

        'main: loop {
            // Check abort
            if config.abort.is_cancelled() {
                yield AgentEvent::Error(AgentError::Aborted);
                break 'main;
            }

            // Drain steer messages between turns
            if turn_index > 0 && let Some(ref mut rx) = config.steer_rx {
                let mut consumed = Vec::new();
                while let Ok(msg) = rx.try_recv() {
                    ctx.messages.push(msg.clone());
                    new_messages.push(msg.clone());
                    consumed.push(msg);
                }
                if !consumed.is_empty() {
                    // Surface the injected steers so the host can persist
                    // them: only run-initial messages otherwise reach the
                    // session, and a steer missing from history silently
                    // diverges the transcript.
                    yield AgentEvent::SteerConsumed { messages: consumed };
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
            let required_tool = (turn_index == 0)
                .then(|| config.run.extension::<RequiredToolCall>())
                .flatten()
                .cloned();
            let tool_choice = if let Some(required) = required_tool.as_ref() {
                if !tool_defs.iter().any(|tool| tool.name == required.tool_name()) {
                    yield AgentEvent::Error(AgentError::Internal(format!(
                        "required run tool '{}' is not available",
                        required.tool_name()
                    )));
                    break 'main;
                }
                ToolChoice::Specific(required.tool_name().to_string())
            } else if tool_defs.is_empty() {
                ToolChoice::None
            } else {
                ToolChoice::Auto
            };

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
                    match crate::time::timeout(
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
                            _ = crate::time::sleep(delay) => {}
                        }
                        attempt += 1;
                        continue;
                    }
                    Err(e) => {
                        // Non-transient (or retries-exhausted) provider error:
                        // give the mounted `ProviderErrorHook` a chance to
                        // repair history and retry the same turn. Unmounted →
                        // the error surfaces unchanged (default behavior).
                        //
                        // Bound: the hook may resend the request at most
                        // `max_provider_error_repairs` times per turn. The
                        // pinned adapter folds outcome-unknown gateway
                        // failures (504 and other unmapped 5xx) into
                        // `InvalidRequest`, so the category alone cannot
                        // prove the request was rejected without side
                        // effects — the bound caps the replay surface until
                        // the adapter exposes a structured status.
                        if let Some(h) = &config.provider_error
                            && matches!(e, LlmError::InvalidRequest(_))
                        {
                            let directive = h
                                .on_provider_error(ProviderErrorCtx {
                                    run: &config.run,
                                    turn_index,
                                    error: &e,
                                    context: &mut ctx,
                                    new_messages: &mut new_messages,
                                })
                                .await;
                            if matches!(directive, ErrorDirective::Retry)
                                && provider_error_repairs < max_provider_error_repairs
                            {
                                provider_error_repairs += 1;
                                continue 'main;
                                // Budget exhausted: surface the error instead
                                // of replaying an unknown-outcome request.
                            }
                        }
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
                let idle_sleep = crate::time::sleep(match idle_timeout {
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
                                        use crate::time::SleepExt;
                                        idle_sleep
                                            .as_mut()
                                            .reset_for(std::time::Duration::from_millis(ms));
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
                        // A stream was already accepted.
                        if had_visible_events {
                            let partial = state.discarded_message();
                            yield AgentEvent::MessageEnd {
                                message_id: partial.message_id.clone(),
                                message: partial,
                                discarded: true,
                            };
                        }
                        // A dropped stream is retryable at REQUEST level: no
                        // tool from this generation has executed (tools run
                        // only after a complete `MessageStop`), so resending
                        // the same request cannot repeat a side effect. This
                        // path previously broke out of the loop, despite the
                        // retry loop's comment claiming it covered stream
                        // consumption — that made every mid-stream drop
                        // unrecoverable.
                        if is_retryable_stream(&e) && retry.can_retry(attempt) {
                            let delay = cap_delay(
                                retry.delay_for_attempt(attempt),
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
                                _ = crate::time::sleep(delay) => {}
                            }
                            attempt += 1;
                            continue;
                        }
                        yield AgentEvent::Error(e);
                        break 'main;
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
                            // Forward per-tool progress to the aggregate
                            // channel. tokio::spawn requires a reactor, which
                            // does not exist on wasm32 — spawn_local on the
                            // current (single-threaded) JS event loop instead.
                            #[cfg(not(target_arch = "wasm32"))]
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
                            #[cfg(target_arch = "wasm32")]
                            wasm_bindgen_futures::spawn_local(async move {
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
                provider_error_repairs = 0;
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

                if let Some(required) = required_tool.as_ref() {
                    yield AgentEvent::Error(AgentError::RequiredToolNotCalled(required.tool_name().to_owned()));
                    break 'main;
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
                    // Steers that arrived after the last turn boundary are
                    // handed back to the host (SteerOrphaned below) instead
                    // of being injected here: an injected user message is
                    // never persisted (only run-initial messages are), so a
                    // "one more turn" would consume them invisibly and the
                    // conversation history would silently diverge. The host
                    // requeues them for the next run, where they persist.
                    let mut orphaned_steers = Vec::new();
                    if let Some(ref mut rx) = config.steer_rx {
                        while let Ok(steer_msg) = rx.try_recv() {
                            orphaned_steers.push(steer_msg);
                        }
                    }
                    if !orphaned_steers.is_empty() {
                        yield AgentEvent::AgentEnd { new_messages };
                        yield AgentEvent::SteerOrphaned { messages: orphaned_steers };
                        return;
                    }
                    // Check follow_up channel
                    if let Some(ref mut rx) = config.follow_up_rx
                        && let Ok(follow_up_msg) = rx.try_recv()
                    {
                        ctx.messages.push(follow_up_msg.clone());
                        new_messages.push(follow_up_msg);
                        turn_index += 1;
                        provider_error_repairs = 0;
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
                provider_error_repairs = 0;
            }
        }

        yield AgentEvent::AgentEnd { new_messages };

        // Final salvage: after the run has ended, drain any steers the loop
        // never consumed (sent during teardown, or before an abort). The
        // receiver dies with this generator; whatever remains here would be
        // silently lost. Hand it to the host layer (harness) so the next run
        // can process it.
        if let Some(ref mut rx) = config.steer_rx {
            let mut orphaned = Vec::new();
            while let Ok(msg) = rx.try_recv() {
                orphaned.push(msg);
            }
            if !orphaned.is_empty() {
                yield AgentEvent::SteerOrphaned { messages: orphaned };
            }
        }
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
            provider_error: None,
            max_provider_error_retries: None,
            steer_rx: None,
            follow_up_rx: None,
            retry: None,
            response_format: None,
        };
        (client, cfg)
    }

    #[tokio::test]
    async fn required_run_tool_rejects_plain_text_and_uses_specific_choice() {
        use llm_adapter::types::ToolChoice;

        let (client, mut cfg) = make_config(vec![MockResponse::text("ACTION: FINISH\nroger")]);
        cfg.tools = vec![Arc::new(NamedTool("send_message"))];
        cfg.run = Arc::new(RunContext::new(
            RunRequest::default().with_extension(RequiredToolCall::new("send_message")),
        ));
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };

        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Error(AgentError::RequiredToolNotCalled(tool))
                if tool == "send_message"
        )));
        let captured = client.captured_requests.lock();
        assert_eq!(captured.len(), 1);
        assert_eq!(
            captured[0].tool_choice(),
            &ToolChoice::Specific("send_message".into())
        );
    }

    #[tokio::test]
    async fn ordinary_run_with_tools_keeps_auto_tool_choice() {
        use llm_adapter::types::ToolChoice;

        let (client, mut cfg) = make_config(vec![MockResponse::text("ordinary reply")]);
        cfg.tools = vec![Arc::new(NamedTool("send_message"))];
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };

        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        assert!(
            events
                .iter()
                .all(|event| !matches!(event, AgentEvent::Error(_)))
        );
        let captured = client.captured_requests.lock();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].tool_choice(), &ToolChoice::Auto);
    }

    #[tokio::test]
    async fn required_run_tool_returns_to_auto_after_the_required_call() {
        use llm_adapter::types::ToolChoice;

        let (client, mut cfg) = make_config(vec![
            MockResponse::tool_use_end_turn("reply-1", "send_message", r#"{}"#),
            MockResponse::text("reply delivered"),
        ]);
        cfg.tools = vec![Arc::new(NamedTool("send_message"))];
        cfg.run = Arc::new(RunContext::new(
            RunRequest::default().with_extension(RequiredToolCall::new("send_message")),
        ));
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };

        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        assert!(
            events
                .iter()
                .all(|event| !matches!(event, AgentEvent::Error(_)))
        );
        let captured = client.captured_requests.lock();
        assert_eq!(captured.len(), 2);
        assert_eq!(
            captured[0].tool_choice(),
            &ToolChoice::Specific("send_message".into())
        );
        assert_eq!(captured[1].tool_choice(), &ToolChoice::Auto);
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
        use parking_lot::Mutex;

        struct CaptureUsage(Arc<Mutex<Vec<TokenUsage>>>);

        impl AfterProviderResponseHook for CaptureUsage {
            fn after_response<'a>(
                &'a self,
                ctx: AfterProviderResponseCtx<'a>,
            ) -> BoxFuture<'a, ()> {
                Box::pin(async move {
                    if let Some(usage) = &ctx.info.usage {
                        self.0.lock().push(usage.clone());
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

        let captured = captured.lock();
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

        let captured = client_raw.captured_requests.lock();
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
    async fn agent_loop_hands_late_steer_to_host_at_natural_stop() {
        // Regression: a steer message arriving after the final turn's
        // boundary was previously dropped with the channel when the loop
        // stopped. The loop now hands it to the host via SteerOrphaned so
        // the host can requeue it for the next run (an injected user message
        // would never persist, silently diverging the conversation history).
        use tokio::sync::mpsc;
        let (steer_tx, steer_rx) = mpsc::channel(16);
        let steer_msg = AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "late steer".into(),
            }],
            timestamp: chrono::Utc::now(),
        });
        steer_tx.send(steer_msg).await.unwrap();
        drop(steer_tx); // no further steers — the loop must stop after turn 1

        let (client, mut cfg) = make_config(vec![
            MockResponse::text("first answer"),
            MockResponse::text("second answer"),
        ]);
        cfg.steer_rx = Some(steer_rx);

        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        assert!(
            !events.iter().any(|e| matches!(e, AgentEvent::Error(_))),
            "no errors expected: {events:?}"
        );
        let agent_end = events
            .iter()
            .find(|e| matches!(e, AgentEvent::AgentEnd { .. }))
            .unwrap();
        if let AgentEvent::AgentEnd { new_messages } = agent_end {
            let has_steer = new_messages.iter().any(|m| {
                if let AgentMessage::User(u) = m {
                    u.content
                        .iter()
                        .any(|c| matches!(c, ContentBlock::Text { text } if text == "late steer"))
                } else {
                    false
                }
            });
            assert!(
                !has_steer,
                "late steer must NOT be consumed invisibly by a follow-up turn: {new_messages:?}"
            );
        }
        let orphaned = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::SteerOrphaned { messages } => Some(messages.clone()),
                _ => None,
            })
            .expect("late steer must be surfaced as SteerOrphaned");
        assert_eq!(orphaned.len(), 1);
        assert!(
            matches!(&orphaned[0], AgentMessage::User(u) if u.content.iter().any(
                |c| matches!(c, ContentBlock::Text { text } if text == "late steer")
            ))
        );
        // Exactly one provider call: the run stopped instead of extending.
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
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
    async fn mid_stream_drop_retries_the_request_and_discards_the_partial() {
        use crate::config::RetryConfig;

        let (client, mut cfg) = make_config(vec![
            MockResponse::stream_dropped_mid("partial", 1165),
            MockResponse::text("recovered"),
        ]);
        cfg.retry = Some(RetryConfig::new(2, 0));
        tokio::time::pause();
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
                timestamp: chrono::Utc::now(),
            })],
        };
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the dropped stream must resend the same request once"
        );
        let retries: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::RetryAttempt { .. }))
            .collect();
        assert_eq!(retries.len(), 1, "one retry must be announced");
        let discarded = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    AgentEvent::MessageEnd {
                        discarded: true,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(discarded, 1, "the partial message must be discarded");
        assert!(
            !events.iter().any(|e| matches!(e, AgentEvent::Error(_))),
            "a recovered retry must not surface an error"
        );
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::MessageEnd {
                discarded: false,
                ..
            }
        )));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AgentEnd { .. }))
        );
    }

    #[tokio::test]
    async fn mid_stream_drop_without_retry_budget_surfaces_the_error() {
        let (client, cfg) = make_config(vec![MockResponse::stream_dropped_mid("partial", 7)]);
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
                timestamp: chrono::Utc::now(),
            })],
        };
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "retry=None must not resend"
        );
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Error(_))));
    }

    /// The safety boundary for request-level retry: a tool call that was
    /// streamed but never completed must not execute, and the retry must not
    /// duplicate a tool that executes on the following attempt.
    #[tokio::test]
    async fn retried_stream_never_executes_a_partial_tool_call() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::config::RetryConfig;

        struct CountingTool {
            count: Arc<AtomicUsize>,
        }
        impl Tool for CountingTool {
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
                self.count.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(ToolResult::full(vec![], serde_json::Value::Null, false)) })
            }
        }

        let dropped_tool = MockResponse {
            model: "mock-model".into(),
            stream_error: None,
            stream_impl: None,
            events: vec![
                Ok(llm_adapter::types::StreamEvent::ContentStart {
                    index: 0,
                    kind: llm_adapter::types::ContentKind::ToolInvocation {
                        id: "never-ran".into(),
                        name: "echo".into(),
                    },
                }),
                Ok(llm_adapter::types::StreamEvent::ToolDelta {
                    index: 0,
                    arguments: "{}".into(),
                }),
                Err(llm_adapter::LlmError::StreamIncomplete {
                    received_chunks: 3,
                    finish_reason: None,
                    kind: llm_adapter::StreamEndKind::TransportError,
                    detail: Some("cause=transport".into()),
                }),
            ],
        };
        let (client, mut cfg) = make_config(vec![
            dropped_tool,
            MockResponse::tool_use("real-call", "echo", "{}"),
            MockResponse::text("done"),
        ]);
        cfg.retry = Some(RetryConfig::new(2, 0));
        tokio::time::pause();

        let calls = Arc::new(AtomicUsize::new(0));
        cfg.tools = vec![Arc::new(CountingTool {
            count: calls.clone(),
        })];
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
                timestamp: chrono::Utc::now(),
            })],
        };
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        assert_eq!(calls.load(Ordering::SeqCst), 1, "tool must execute once");
        let executed_ids: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolExecutionStart { tool_use_id, .. } => Some(tool_use_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(executed_ids, vec!["real-call".to_string()]);
    }

    /// A completed earlier turn may execute tools. A later generation that
    /// drops mid-stream is retried at request level, which must not replay the
    /// already-executed tool turn.
    #[tokio::test]
    async fn retry_after_completed_tool_turn_does_not_repeat_the_tool() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::config::RetryConfig;

        struct CountingTool {
            count: Arc<AtomicUsize>,
        }
        impl Tool for CountingTool {
            fn name(&self) -> &str {
                "echo"
            }
            fn description(&self) -> &str {
                ""
            }
            fn parameters_schema(&self) -> &serde_json::Value {
                static S: std::sync::LazyLock<serde_json::Value> =
                    std::sync::LazyLock::new(|| serde_json::json!({}));
                &S
            }
            fn execute<'a>(
                &'a self,
                _: serde_json::Value,
                _: &'a ToolContext,
            ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
                self.count.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(ToolResult::full(vec![], serde_json::Value::Null, false)) })
            }
        }

        let (client, mut cfg) = make_config(vec![
            MockResponse::tool_use("first-call", "echo", "{}"),
            MockResponse::stream_dropped_mid("partial", 7),
            MockResponse::text("done"),
        ]);
        cfg.retry = Some(RetryConfig::new(2, 0));
        tokio::time::pause();

        let calls = Arc::new(AtomicUsize::new(0));
        cfg.tools = vec![Arc::new(CountingTool {
            count: calls.clone(),
        })];
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
                timestamp: chrono::Utc::now(),
            })],
        };
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the completed tool turn must not be replayed"
        );
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "tool turn + dropped generation + retried generation"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::RetryAttempt { .. }))
                .count(),
            1
        );
        assert!(
            !events.iter().any(|e| matches!(e, AgentEvent::Error(_))),
            "the retry must recover without surfacing an error"
        );
    }

    /// Once `MessageStop` completed the generation, a later stream error is
    /// not a dropped generation and must not trigger a retry.
    #[tokio::test]
    async fn error_after_message_stop_does_not_retry_completed_generation() {
        use crate::config::RetryConfig;

        let completed_then_error = MockResponse {
            model: "mock-model".into(),
            stream_error: None,
            stream_impl: None,
            events: vec![
                Ok(llm_adapter::types::StreamEvent::ContentStart {
                    index: 0,
                    kind: llm_adapter::types::ContentKind::Text,
                }),
                Ok(llm_adapter::types::StreamEvent::TextDelta {
                    index: 0,
                    text: "complete".into(),
                }),
                Ok(llm_adapter::types::StreamEvent::ContentStop {
                    index: 0,
                    signature: None,
                }),
                Ok(llm_adapter::types::StreamEvent::MessageStop {
                    stop_reason: llm_adapter::types::StopReason::EndTurn,
                    usage: llm_adapter::types::Usage::default(),
                }),
                Err(llm_adapter::LlmError::StreamIncomplete {
                    received_chunks: 1,
                    finish_reason: None,
                    kind: llm_adapter::StreamEndKind::TransportError,
                    detail: Some("cause=transport".into()),
                }),
            ],
        };
        let (client, mut cfg) = make_config(vec![
            completed_then_error,
            MockResponse::text("must-not-run"),
        ]);
        cfg.retry = Some(RetryConfig::new(2, 0));
        tokio::time::pause();
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
                timestamp: chrono::Utc::now(),
            })],
        };
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a completed generation must never be resent"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::RetryAttempt { .. }))
        );
        assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error(_))));
    }

    /// Cancelling during the retry backoff must abort immediately, before a
    /// second request is sent.
    #[tokio::test]
    async fn stream_retry_backoff_is_cancellable() {
        use crate::config::RetryConfig;

        let (client, mut cfg) = make_config(vec![
            MockResponse::stream_dropped_mid("partial", 7),
            MockResponse::text("must-not-run"),
        ]);
        cfg.retry = Some(RetryConfig::new(2, 60_000));
        let abort = cfg.abort.clone();
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(llm_harness_types::UserMessage {
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
                timestamp: chrono::Utc::now(),
            })],
        };

        let stream = agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg);
        tokio::pin!(stream);
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            let is_retry = matches!(event, AgentEvent::RetryAttempt { .. });
            events.push(event);
            if is_retry {
                abort.cancel();
            }
        }

        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "cancellation during backoff must not send a second request"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Error(AgentError::Aborted))),
            "cancelled backoff must surface Aborted"
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

        let captured = client.captured_requests.lock();
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
    async fn stream_idle_timeout_does_not_retry_accepted_request() {
        let (client, mut cfg) = make_config(vec![
            MockResponse::stall_after(1),
            MockResponse::text("must not be requested"),
        ]);
        cfg.retry = Some(crate::config::RetryConfig::new(3, 10_000));
        cfg.stream_options.stream_idle_timeout_ms = Some(30);

        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        tokio::time::pause();
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::RetryAttempt { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Error(AgentError::StreamIdle { .. })))
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::MessageStart { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    e,
                    AgentEvent::MessageEnd {
                        discarded: true,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AgentEnd { .. }))
        );
    }

    #[tokio::test]
    async fn stream_idle_timeout_ignores_retry_budget() {
        let (client, mut cfg) = make_config(vec![
            MockResponse::stall_after(1),
            MockResponse::text("must not be requested"),
        ]);
        cfg.retry = Some(crate::config::RetryConfig::new(3, 10_000));
        cfg.stream_options.stream_idle_timeout_ms = Some(30);
        cfg.stream_options.max_retries = Some(99);
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        tokio::time::pause();
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::RetryAttempt { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Error(AgentError::StreamIdle { .. })))
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::MessageStart { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    e,
                    AgentEvent::MessageEnd {
                        discarded: true,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AgentEnd { .. }))
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
    async fn idle_timeout_does_not_consume_later_success() {
        let (client, mut cfg) = make_config(vec![
            MockResponse::stall_after(0),
            MockResponse::text("must not be requested"),
        ]);
        cfg.retry = Some(crate::config::RetryConfig::new(3, 10_000));
        cfg.stream_options.stream_idle_timeout_ms = Some(30);

        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        tokio::time::pause();
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::RetryAttempt { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Error(AgentError::StreamIdle { .. })))
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::MessageStart { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    e,
                    AgentEvent::MessageEnd {
                        discarded: true,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AgentEnd { .. }))
        );
    }

    #[tokio::test]
    async fn idle_failure_closes_partial_message_without_replay() {
        let (client, mut cfg) = make_config(vec![
            MockResponse::stall_after(2),
            MockResponse::text("must not be requested"),
        ]);
        cfg.retry = Some(crate::config::RetryConfig::new(3, 10_000));
        cfg.stream_options.stream_idle_timeout_ms = Some(30);

        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        tokio::time::pause();
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::RetryAttempt { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Error(AgentError::StreamIdle { .. })))
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::MessageStart { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    e,
                    AgentEvent::MessageEnd {
                        discarded: true,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AgentEnd { .. }))
        );
    }

    #[tokio::test]
    async fn idle_failure_closes_started_message_without_replay() {
        let (client, mut cfg) = make_config(vec![
            MockResponse::stall_after(1),
            MockResponse::text("must not be requested"),
        ]);
        cfg.retry = Some(crate::config::RetryConfig::new(3, 10_000));
        cfg.stream_options.stream_idle_timeout_ms = Some(30);

        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        tokio::time::pause();
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::RetryAttempt { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Error(AgentError::StreamIdle { .. })))
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::MessageStart { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    e,
                    AgentEvent::MessageEnd {
                        discarded: true,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AgentEnd { .. }))
        );
    }

    #[tokio::test]
    async fn idle_failure_has_no_retry_sleep() {
        let (client, mut cfg) = make_config(vec![
            MockResponse::stall_after(2),
            MockResponse::text("must not be requested"),
        ]);
        cfg.retry = Some(crate::config::RetryConfig::new(3, 10_000));
        cfg.stream_options.stream_idle_timeout_ms = Some(30);
        cfg.stream_options.max_retries = Some(99);
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        tokio::time::pause();
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::RetryAttempt { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Error(AgentError::StreamIdle { .. })))
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::MessageStart { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    e,
                    AgentEvent::MessageEnd {
                        discarded: true,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AgentEnd { .. }))
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

        let captured = client.captured_requests.lock();
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
        let captured = client_raw.captured_requests.lock();
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
        let captured = client_raw.captured_requests.lock();
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
                    kind: llm_adapter::StreamEndKind::TransportError,
                    detail: Some("cause=transport".into()),
                },
                |k| {
                    matches!(
                        k,
                        ProviderErrorKind::StreamIncomplete {
                            received_chunks: 3,
                            finish_reason: None,
                            detail: Some(detail),
                            ..
                        } if detail == "cause=transport"
                    )
                },
            ),
            (
                E::StreamIncomplete {
                    received_chunks: 5,
                    finish_reason: Some("length".into()),
                    kind: llm_adapter::StreamEndKind::DoneWithoutFinishReason,
                    detail: None,
                },
                |k| matches!(k, ProviderErrorKind::StreamIncomplete { received_chunks: 5, finish_reason: Some(fr), .. } if fr == "length"),
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

    // ── Vision degradation (issue #145 gaps 2+3) ────────────────────────────

    use llm_adapter::LlmError;
    use llm_harness_types::{
        ImageSource, PersistedToolProjection, ProjectionMetadata, ToolProjectionKind,
        ToolResultMessage,
    };

    fn image_user_message() -> AgentMessage {
        AgentMessage::User(UserMessage {
            content: vec![
                ContentBlock::Text {
                    text: "what is in this picture?".into(),
                },
                ContentBlock::Image {
                    source: ImageSource::Url {
                        url: "https://example.com/pic.png".into(),
                    },
                },
            ],
            timestamp: chrono::Utc::now(),
        })
    }

    fn tool_result_with_image() -> AgentMessage {
        AgentMessage::ToolResult(ToolResultMessage {
            tool_use_id: "call_1".into(),
            content: vec![
                DataBlock::Text {
                    text: "screenshot".into(),
                    mime_type: None,
                },
                DataBlock::Image {
                    source: ImageSource::Url {
                        url: "https://example.com/shot.png".into(),
                    },
                },
            ],
            is_error: false,
            projection: PersistedToolProjection {
                kind: ToolProjectionKind::Full,
                metadata: ProjectionMetadata::default(),
            },
            timestamp: chrono::Utc::now(),
        })
    }

    fn image_block_count(req: &llm_adapter::types::ChatRequest) -> usize {
        req.messages()
            .iter()
            .map(|m| match m {
                llm_adapter::types::Message::User(content) => content
                    .iter()
                    .filter(|c| matches!(c, llm_adapter::types::RequestContent::Image(_)))
                    .count(),
                llm_adapter::types::Message::Tool { content, .. } => content
                    .iter()
                    .filter(|c| matches!(c, llm_adapter::types::RequestContent::Image(_)))
                    .count(),
                _ => 0,
            })
            .sum()
    }

    /// Test hook: strips every Image/Document block from both disjoint views
    /// (mirroring the strategy crate's `VisionDegradeHook`), retries once,
    /// then surfaces. Stand-in so loop tests don't depend on llm-harness-strategy.
    struct StripVisionHook {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl StripVisionHook {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
            })
        }
    }

    impl ProviderErrorHook for StripVisionHook {
        fn on_provider_error<'a>(
            &'a self,
            ctx: ProviderErrorCtx<'a>,
        ) -> BoxFuture<'a, ErrorDirective> {
            Box::pin(async move {
                let attempt = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if attempt > 0 {
                    return ErrorDirective::Surface;
                }
                assert!(matches!(ctx.error, LlmError::InvalidRequest(_)));
                assert_eq!(ctx.turn_index, 0);
                let stripped = strip_test_visual_blocks(ctx.context.messages.as_mut_slice())
                    + strip_test_visual_blocks(ctx.new_messages);
                if stripped == 0 {
                    // Nothing repairable (no visual blocks) — surface unchanged.
                    return ErrorDirective::Surface;
                }
                ErrorDirective::Retry
            })
        }
    }

    /// Strips Image/Document blocks from test messages, returning the count.
    fn strip_test_visual_blocks(messages: &mut [AgentMessage]) -> usize {
        let mut stripped = 0usize;
        for msg in messages.iter_mut() {
            match msg {
                AgentMessage::ToolResult(t) => {
                    let before = t.content.len();
                    t.content.retain(|b| {
                        !matches!(b, DataBlock::Image { .. } | DataBlock::Document { .. })
                    });
                    stripped += before - t.content.len();
                }
                AgentMessage::User(u) => {
                    let before = u.content.len();
                    u.content.retain(|b| {
                        !matches!(
                            b,
                            ContentBlock::Image { .. } | ContentBlock::Document { .. }
                        )
                    });
                    stripped += before - u.content.len();
                }
                _ => {}
            }
        }
        stripped
    }

    #[tokio::test]
    async fn provider_error_hook_repairs_history_and_retries_same_turn() {
        // Turn 1: provider rejects the image-carrying request; the mounted
        // hook strips both views and returns Retry (same `turn_index`).
        // Turn 2: normal completion.
        let (client, mut cfg) = make_config(vec![
            MockResponse {
                events: vec![],
                model: String::new(),
                stream_error: Some(LlmError::InvalidRequest(
                    "model does not support image input".into(),
                )),
                stream_impl: None,
            },
            MockResponse::text("I cannot see images."),
        ]);
        let hook = StripVisionHook::new();
        cfg.provider_error = Some(hook.clone());
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![image_user_message(), tool_result_with_image()],
        };
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        // Run ended normally, not in error.
        assert!(
            events.iter().all(|e| !matches!(e, AgentEvent::Error(_))),
            "repaired run must not surface as a terminal Error"
        );
        assert!(
            events.iter().any(|e| e
                .as_final_answer()
                .map(|(_, _, t)| t == "I cannot see images.")
                .unwrap_or(false)),
            "run should complete after repair"
        );

        // Two provider calls: first with images, second without.
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        let captured = client.captured_requests.lock();
        assert_eq!(captured.len(), 2);
        assert_eq!(
            image_block_count(&captured[0]),
            2,
            "first request must still carry both image blocks"
        );
        assert_eq!(
            image_block_count(&captured[1]),
            0,
            "retried request must be image-free"
        );
        assert_eq!(
            hook.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "hook fires exactly once"
        );
    }

    #[tokio::test]
    async fn unmounted_provider_error_hook_surfaces_invalid_request_unchanged() {
        // Default behavior (no hook): the InvalidRequest surfaces as a
        // terminal Error after a single provider call — no hidden retry.
        let (client, cfg) = make_config(vec![MockResponse {
            events: vec![],
            model: String::new(),
            stream_error: Some(LlmError::InvalidRequest(
                "model does not support image input".into(),
            )),
            stream_impl: None,
        }]);
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![image_user_message(), tool_result_with_image()],
        };
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Error(_))),
            "unmounted hook must surface the error unchanged"
        );
        assert!(
            events.iter().all(|e| e.as_final_answer().is_none()),
            "no final answer when the run errors out"
        );
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one provider call — no silent retry"
        );
    }

    #[tokio::test]
    async fn provider_error_hook_one_shot_guard_surfaces_second_failure() {
        // Provider keeps rejecting; the hook's own one-shot guard must not
        // loop — the second failure surfaces as a terminal Error.
        let (client, mut cfg) = make_config(vec![
            MockResponse {
                events: vec![],
                model: String::new(),
                stream_error: Some(LlmError::InvalidRequest("no vision".into())),
                stream_impl: None,
            },
            MockResponse {
                events: vec![],
                model: String::new(),
                stream_error: Some(LlmError::InvalidRequest(
                    "still failing for another reason".into(),
                )),
                stream_impl: None,
            },
        ]);
        let hook = StripVisionHook::new();
        cfg.provider_error = Some(hook.clone());
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![image_user_message()],
        };
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Error(_))),
            "second failure must surface as terminal Error"
        );
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "exactly one repair retry"
        );
        assert_eq!(
            hook.calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "hook re-consulted on the second failure, then surfaces"
        );
    }

    #[tokio::test]
    async fn text_only_invalid_request_does_not_invoke_provider_error_hook() {
        // An InvalidRequest with no visual blocks in history: a Surface-only
        // hook must still be consulted (the loop cannot know what a hook
        // considers repairable), and the error surfaces when it declines.
        let (client, mut cfg) = make_config(vec![MockResponse {
            events: vec![],
            model: String::new(),
            stream_error: Some(LlmError::InvalidRequest("bad parameter".into())),
            stream_impl: None,
        }]);
        let hook = StripVisionHook::new();
        cfg.provider_error = Some(hook.clone());
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(UserMessage {
                content: vec![ContentBlock::Text { text: "hi".into() }],
                timestamp: chrono::Utc::now(),
            })],
        };
        let events: Vec<AgentEvent> =
            agent_loop(Arc::clone(&client) as Arc<dyn Provider>, ctx, cfg)
                .collect()
                .await;

        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Error(_))),
            "hook cannot repair (no visual blocks) → error surfaces"
        );
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no retry when the hook declines"
        );
        assert_eq!(
            hook.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "hook consulted once"
        );
    }
}
