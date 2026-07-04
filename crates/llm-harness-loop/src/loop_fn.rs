use std::{collections::HashSet, sync::Arc};

use futures::{Stream, StreamExt};
use llm_adapter::provider::Provider;
use llm_harness_types::*;
use uuid::Uuid;

use crate::config::LoopConfig;
use crate::final_answer::{
    FinalAnswerMissingBehavior, FinalAnswerMode, FinalAnswerToolConfig, builtin_final_answer_tool,
};
use crate::stream_state::TerminalTextClassification;

type ToolSelection = (Vec<Arc<dyn Tool>>, Option<FinalAnswerToolConfig>);

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

    if answer_cfg.name.trim().is_empty() {
        return Err(AgentError::Internal(
            "final-answer tool name must not be empty".into(),
        ));
    }
    if tools.iter().any(|tool| tool.name() == answer_cfg.name) {
        return Err(AgentError::Internal(format!(
            "final-answer tool name '{}' conflicts with a registered tool",
            answer_cfg.name
        )));
    }

    let builtin = builtin_final_answer_tool(&answer_cfg);
    let mut registered = tools.to_vec();
    registered.push(builtin.clone());

    let mut selected = active_tool_subset(&registered, active)?;
    if !selected.iter().any(|tool| tool.name() == answer_cfg.name) {
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
    results: &[(String, Result<ToolResult, ToolError>)],
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
                match h.transform(ctx).await {
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
            let req = req_b.build();

            // BeforeProviderRequestHook: allow callers to modify stream options.
            let mut stream_opts = config.stream_options.clone();
            if let Some(h) = &config.before_provider_request {
                h.before_request(&mut stream_opts).await;
            }

            // Call LLM (with optional retry on transient errors)
            let mut handle = {
                use crate::config::{is_retryable, RetryConfig};
                let mut retry = config.retry.as_ref().cloned().unwrap_or(RetryConfig {
                    max_retries: 0,
                    base_delay_ms: 0,
                });
                if let Some(max_retries) = stream_opts.max_retries {
                    retry.max_retries = max_retries;
                }
                let mut attempt = 0u32;
                loop {
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
                                yield AgentEvent::Error(AgentError::Provider(format!(
                                    "provider request timed out after {timeout_ms}ms"
                                )));
                                break 'main;
                            }
                        }
                    } else {
                        call.await
                    };

                    match response {
                        Ok(h) => break h,
                        Err(e) if is_retryable(&e) && retry.can_retry(attempt) => {
                            let mut delay = retry.delay_for(attempt, &e);
                            if let Some(max_delay_ms) = stream_opts.max_retry_delay_ms {
                                delay = delay.min(std::time::Duration::from_millis(max_delay_ms));
                            }
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
                        }
                        Err(e) => {
                            yield AgentEvent::Error(AgentError::Provider(e.to_string()));
                            break 'main;
                        }
                    }
                }
            };

            // Emit MessageStart
            let mut state = StreamingState::new_with_terminal_text_classification(
                handle.model().to_owned(),
                turn_id,
                terminal_text_classification(&config.final_answer_mode),
            );
            let message_id = state.message_id.clone();
            yield AgentEvent::MessageStart { message_id: message_id.clone() };

            // Process stream events
            let mut final_message: Option<AssistantMessage> = None;
            loop {
                match handle.events().next().await {
                    Some(Ok(adapter_event)) => {
                        for agent_event in state.process(adapter_event) {
                            if let AgentEvent::MessageEnd { ref message, .. } = agent_event {
                                final_message = Some(message.clone());
                            }
                            yield agent_event;
                        }
                    }
                    Some(Err(e)) => {
                        yield AgentEvent::Error(AgentError::Provider(e.to_string()));
                        break 'main;
                    }
                    None => break,
                }
            }

            let assistant_msg = match final_message {
                Some(m) => m,
                None => {
                    yield AgentEvent::Error(AgentError::Internal(
                        "stream ended without MessageEnd".into(),
                    ));
                    break 'main;
                }
            };

            // AfterProviderResponseHook: observation hook for quota tracking, cost monitoring.
            if let Some(h) = &config.after_provider_response {
                let info = ProviderResponseInfo {
                    status_code: None,
                    response_headers: vec![],
                    usage: assistant_msg.usage.clone(),
                    latency_ms: 0,
                    model: assistant_msg.model.clone(),
                    provider: assistant_msg.provider.clone(),
                };
                h.after_response(&info).await;
            }

            let assistant_agent_msg = AgentMessage::Assistant(assistant_msg.clone());
            ctx.messages.push(assistant_agent_msg.clone());
            new_messages.push(assistant_agent_msg);

            let stop_reason = assistant_msg.stop_reason.unwrap_or(StopReason::EndTurn);

            if stop_reason == StopReason::ToolUse {
                // Collect tool-use blocks from the assistant message
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
                let env = config.env.clone();
                let abort = config.abort.clone();
                let turn_idx = turn_index;

                // Aggregate per-tool update channels so partial tool results can be
                // tagged with their originating tool_use_id before being emitted.
                let (update_tx, mut update_rx) = mpsc::channel::<(String, ToolResult)>(64);

                let batch = execute_tool_batch(
                    calls,
                    {
                        let env = env.clone();
                        let abort = abort.clone();
                        let assistant_arc = assistant_arc.clone();
                        let update_tx = update_tx.clone();
                        move |tool_use_id| {
                            let (tool_update_tx, mut tool_update_rx) =
                                mpsc::channel::<ToolResult>(16);
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
                        Err(ToolError::Execution(format!("Unknown tool: {name}"))),
                    ));
                }

                // Emit ToolExecutionEnd and build ToolResult messages
                for (id, result) in &results {
                    yield AgentEvent::ToolExecutionEnd {
                        tool_use_id: id.clone(),
                        result: match result {
                            Ok(r) => Ok(r.clone()),
                            Err(e) => Err(ToolError::Execution(e.to_string())),
                        },
                    };
                    let (content, is_error) = match result {
                        Ok(r) => (r.content.clone(), false),
                        Err(e) => (
                            vec![ContentBlock::Text { text: e.to_string() }],
                            true,
                        ),
                    };
                    let tool_result_msg = AgentMessage::ToolResult(ToolResultMessage {
                        tool_use_id: id.clone(),
                        content,
                        is_error,
                        timestamp: chrono::Utc::now(),
                    });
                    ctx.messages.push(tool_result_msg.clone());
                    new_messages.push(tool_result_msg);
                }

                let final_answer_message = current_final_answer_tool
                    .as_ref()
                    .and_then(|cfg| {
                        final_answer_text_from_results(&cfg.name, &tool_calls, &results)
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
                    let message_id = final_message.message_id.clone();
                    yield AgentEvent::MessageStart {
                        message_id: message_id.clone(),
                    };
                    yield AgentEvent::MessageEnd {
                        message_id,
                        message: final_message.clone(),
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
            }

            if let FinalAnswerMode::Tool(cfg) = &config.final_answer_mode
                && cfg.missing_behavior == FinalAnswerMissingBehavior::Error
            {
                yield AgentEvent::Error(AgentError::Internal(format!(
                    "final-answer tool '{}' was required but the model stopped without calling it",
                    cfg.name
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

        yield AgentEvent::AgentEnd { new_messages };
    }
}

#[cfg(test)]
#[cfg(feature = "test-utils")]
mod tests {
    use super::{active_tool_subset, apply_next_turn_directive};

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
        ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
            Box::pin(async {
                Ok(ToolResult {
                    content: vec![],
                    details: serde_json::Value::Null,
                    terminate: false,
                })
            })
        }
    }

    fn make_config(responses: Vec<MockResponse>) -> (Arc<MockLlmClient>, LoopConfig) {
        let client = Arc::new(MockLlmClient::new(responses));
        let cfg = LoopConfig {
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
            auth: None,
            steer_rx: None,
            follow_up_rx: None,
            retry: None,
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
            },
        )
        .unwrap();

        assert_eq!(cfg.temperature, Some(0.7), "None directive must preserve current");
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
            },
        )
        .unwrap();

        assert_eq!(cfg.temperature, Some(0.3), "Some(Some(0.3)) must set value");
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
            ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
                self.called.store(true, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async {
                    Ok(ToolResult {
                        content: vec![],
                        details: serde_json::Value::Null,
                        terminate: false,
                    })
                })
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
    async fn final_answer_tool_name_conflict_errors_before_provider_call() {
        let (client, mut cfg) = make_config(vec![MockResponse::text("unused")]);
        cfg.tools = vec![Arc::new(NamedTool("final_answer"))];
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

        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Error(AgentError::Internal(message))
                if message.contains("conflicts")
                    && message.contains("final_answer")
        )));
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
            ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
                Box::pin(async move {
                    ctx.update_tx
                        .send(ToolResult {
                            content: vec![ContentBlock::Text {
                                text: "partial".into(),
                            }],
                            details: serde_json::json!({"step": 1}),
                            terminate: false,
                        })
                        .await
                        .unwrap();

                    Ok(ToolResult {
                        content: vec![ContentBlock::Text {
                            text: "done".into(),
                        }],
                        details: serde_json::Value::Null,
                        terminate: true,
                    })
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
                        && matches!(&partial.content[0], ContentBlock::Text { text } if text == "partial")
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
            ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
                Box::pin(async {
                    Ok(ToolResult {
                        content: vec![],
                        details: serde_json::Value::Null,
                        terminate: true,
                    })
                })
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
}
