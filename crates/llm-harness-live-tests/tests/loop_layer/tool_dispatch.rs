//! Tests: tool dispatch, parallel tools, error propagation.

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_live_tests::{
    EventCapture, LiveProvider, MULTI_TURN, assert_tool_was_called, make_harness_with, with_timeout,
};
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolResult};

// ── Test tools ───────────────────────────────────────────────────────────────

struct EchoTool {
    schema: serde_json::Value,
}

impl EchoTool {
    fn new() -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "input": { "type": "string", "description": "Text to echo" }
                },
                "required": ["input"]
            }),
        }
    }
}

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes the input text back."
    }
    fn parameters_schema(&self) -> &serde_json::Value {
        &self.schema
    }
    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        let input = args
            .get("input")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Box::pin(async move {
            Ok(ToolResult::full(
                vec![DataBlock::text(format!("echo:{}", input))],
                serde_json::Value::Null,
                false,
            ))
        })
    }
}

struct ErrorTool {
    schema: serde_json::Value,
}

impl ErrorTool {
    fn new() -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "reason": { "type": "string", "description": "Why this tool is being called" }
                },
                "required": ["reason"]
            }),
        }
    }
}

impl Tool for ErrorTool {
    fn name(&self) -> &str {
        "fail"
    }
    fn description(&self) -> &str {
        "Always returns an error. Use for testing error propagation."
    }
    fn parameters_schema(&self) -> &serde_json::Value {
        &self.schema
    }
    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            Err(ToolFailure::new(
                "execution_failed",
                "intentional test error",
            ))
        })
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Single tool call round-trip: LLM calls tool → tool executes → result written → LLM continues.
#[tokio::test]
async fn single_tool_call_round_trips() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let (harness, _tmp) = make_harness_with(entry, |b| {
        b.tool(Arc::new(EchoTool::new())).system_prompt(Some(
            "You must call tools using the function calling mechanism, not by describing \
                 the call in text. Always invoke tools via tool_call."
                .to_string(),
        ))
    })
    .await;

    let capture = EventCapture::subscribe(&harness);

    with_timeout(MULTI_TURN, harness.prompt("Echo the word 'hello'."))
        .await
        .expect("prompt should succeed");

    capture.settle().await;
    let events = capture.events();
    assert_tool_was_called(&events, "echo");
}

/// Tool error propagates to LLM — the LLM should see the error and continue.
#[tokio::test]
async fn tool_error_propagates_to_llm() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let (harness, _tmp) = make_harness_with(entry, |b| {
        b.tool(Arc::new(ErrorTool::new()))
            .tool(Arc::new(EchoTool::new()))
            .system_prompt(Some(
                "You MUST call tools by emitting tool_call, not by describing the call in text. \
                 Always invoke tools via the function calling mechanism."
                    .to_string(),
            ))
    })
    .await;

    let capture = EventCapture::subscribe(&harness);

    with_timeout(
        MULTI_TURN,
        harness.prompt(
            "Call the fail tool with reason 'testing error propagation'. \
             After it errors, call the echo tool with input 'recovered'.",
        ),
    )
    .await
    .expect("prompt should succeed");
    // The LLM should continue after the error (not crash).
    let text = capture.texts();
    assert!(!text.is_empty(), "LLM should produce text after tool error");
}
