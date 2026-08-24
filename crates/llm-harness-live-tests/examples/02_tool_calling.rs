//! 02 — Tool Calling: register tools and let the LLM discover and call them.
//!
//! Demonstrates:
//! - Defining a custom Tool implementation
//! - Registering tools on HarnessBuilder
//! - The LLM discovers tools, calls them, and incorporates results
//! - Parallel tool calls (two tools called in one turn)
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 02_tool_calling

use std::sync::Arc;
use std::time::SystemTime;

use futures::future::BoxFuture;
use llm_harness_live_tests::{
    EventCapture, LiveProvider, MULTI_TURN, make_harness_with, with_timeout,
};
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolResult};

// ── Weather tool ─────────────────────────────────────────────────────────────

struct WeatherTool {
    schema: serde_json::Value,
}

impl WeatherTool {
    fn new() -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "city": { "type": "string", "description": "City name" }
                },
                "required": ["city"]
            }),
        }
    }
}

impl Tool for WeatherTool {
    fn name(&self) -> &str {
        "get_weather"
    }

    fn description(&self) -> &str {
        "Get current weather for a city. Returns temperature and condition."
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        &self.schema
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        let city = args
            .get("city")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        Box::pin(async move {
            Ok(ToolResult::full(
                vec![DataBlock::text(format!(
                    "The weather in {} is sunny, 22°C.",
                    city
                ))],
                serde_json::Value::Null,
                false,
            ))
        })
    }
}

// ── Time tool ────────────────────────────────────────────────────────────────

struct TimeTool {
    schema: serde_json::Value,
}

impl TimeTool {
    fn new() -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "timezone": {
                        "type": "string",
                        "description": "Timezone name, e.g. 'UTC' or 'Asia/Tokyo'"
                    }
                },
                "required": ["timezone"]
            }),
        }
    }
}

impl Tool for TimeTool {
    fn name(&self) -> &str {
        "get_time"
    }

    fn description(&self) -> &str {
        "Get the current time in ISO 8601 format."
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
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            Ok(ToolResult::full(
                vec![DataBlock::text(format!("Current UNIX timestamp: {}", now))],
                serde_json::Value::Null,
                false,
            ))
        })
    }
}

#[tokio::main]
async fn main() {
    println!("=== 02: Tool Calling ===\n");

    let provider = LiveProvider::from_env();
    let entry = provider
        .openai
        .as_ref()
        .expect("set OPENAI_API_KEY to run this example");

    println!("Provider: {} | Model: {}", entry.name, entry.model);

    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        b.tool(Arc::new(WeatherTool::new()))
            .tool(Arc::new(TimeTool::new()))
            .system_prompt(Some(
                "You are a helpful assistant. Use the available tools to answer questions."
                    .to_string(),
            ))
    })
    .await;

    println!("Tools registered: get_weather, get_time\n");

    let capture = EventCapture::subscribe(&harness);

    // Ask about weather — should trigger tool call.
    let prompt = "What's the weather in Tokyo and what time is it now?";
    println!("Prompt: \"{}\"\n", prompt);
    with_timeout(MULTI_TURN, harness.prompt(prompt))
        .await
        .expect("prompt failed");

    capture.settle().await;

    // Show which tools were called.
    let tool_calls = capture.tool_calls();
    println!("\n--- Tools called ---");
    for name in &tool_calls {
        println!("  - {}", name);
    }

    // Show the response.
    let text = capture.texts();
    println!("\n--- Response ---\n{}\n", text);

    let usage = harness.usage();
    println!(
        "Tokens: {} in / {} out",
        usage.total_input_tokens, usage.total_output_tokens
    );
}
