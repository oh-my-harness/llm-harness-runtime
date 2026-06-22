//! Event-to-terminal rendering for coding-agent.
//!
//! Subscribes to `AgentHarnessEvent` broadcast and renders:
//! - `TextDelta` → streaming markdown render (see [`StreamingRenderer`])
//! - `ToolCallStart` → "🔧 tool_name { args }" on a dedicated line
//! - `ToolCallEnd` → "✓ <summary>" or "✗ <error>" on a dedicated line
//! - `RetryAttempt` → "⟳ retry N/M in Xs: <reason>" warning line
//! - `Error` → stderr
//! - `PhaseChange { to: Idle }` → flush remaining buffer and break

use std::io::Write;
use std::sync::Arc;

use llm_harness_agent::{AgentHarnessEvent, HarnessToolCallResult};
use llm_harness_types::{AgentEvent, ContentBlock, HarnessPhase};
use tokio::sync::broadcast;

pub mod renderer;
pub use renderer::StreamingRenderer;

// ── ANSI color helpers ────────────────────────────────────────────────────────

/// ANSI escape codes used for tool-call annotation lines.
mod ansi {
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const GREEN: &str = "\x1b[32m";
    pub const RED: &str = "\x1b[31m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const CYAN: &str = "\x1b[36m";
    pub const RESET: &str = "\x1b[0m";
}

/// Maximum number of characters shown for tool arguments inline.
const MAX_ARGS_DISPLAY: usize = 120;
/// Maximum number of characters shown for tool result content.
const MAX_RESULT_DISPLAY: usize = 200;

// ── Tool-call annotation rendering ───────────────────────────────────────────

/// Print a single-line "tool call started" annotation.
///
/// Format: `🔧 <tool_name>  <compact-args>`
///
/// Printed to stdout so it appears in-line with assistant text.
fn print_tool_start(tool_name: &str, args: &serde_json::Value) {
    let args_str = format_args_compact(args);
    println!(
        "\n{cyan}{bold}🔧 {name}{reset}  {dim}{args}{reset}",
        cyan = ansi::CYAN,
        bold = ansi::BOLD,
        name = tool_name,
        reset = ansi::RESET,
        dim = ansi::DIM,
        args = args_str,
    );
    let _ = std::io::stdout().flush();
}

/// Print a single-line "tool call ended" annotation.
///
/// - Success: `  ✓ <content-summary>`
/// - Failure: `  ✗ <error-message>`
fn print_tool_end(tool_name: &str, result: &HarnessToolCallResult) {
    if result.is_error {
        // Extract error text from content blocks.
        let msg = extract_text_from_content(&result.content, MAX_RESULT_DISPLAY);
        println!(
            "{red}{bold}  ✗ {name}: {msg}{reset}",
            red = ansi::RED,
            bold = ansi::BOLD,
            name = tool_name,
            msg = msg,
            reset = ansi::RESET,
        );
    } else {
        let summary = summarize_result(&result.content, MAX_RESULT_DISPLAY);
        println!(
            "{green}  ✓ {name}: {summary}{reset}",
            green = ansi::GREEN,
            name = tool_name,
            summary = summary,
            reset = ansi::RESET,
        );
    }
    let _ = std::io::stdout().flush();
}

/// Print a "retrying" warning line.
fn print_retry(attempt: u32, max_retries: u32, delay_ms: u64, error: &str) {
    let secs = delay_ms as f64 / 1000.0;
    println!(
        "\n{yellow}⟳ retry {n}/{max} in {secs:.1}s: {err}{reset}",
        yellow = ansi::YELLOW,
        n = attempt + 1,
        max = max_retries,
        secs = secs,
        err = truncate(error, 120),
        reset = ansi::RESET,
    );
    let _ = std::io::stdout().flush();
}

// ── Formatting helpers ────────────────────────────────────────────────────────

/// Render tool arguments as a compact single-line string, truncated if long.
fn format_args_compact(args: &serde_json::Value) -> String {
    // For objects, render as "key=value, key=value" for readability.
    if let Some(obj) = args.as_object() {
        let parts: Vec<String> = obj
            .iter()
            .map(|(k, v)| {
                let val = match v {
                    serde_json::Value::String(s) => truncate(s, 60).to_string(),
                    other => {
                        let s = other.to_string();
                        truncate(&s, 60).to_string()
                    }
                };
                format!("{k}={val}")
            })
            .collect();
        let joined = parts.join("  ");
        truncate(&joined, MAX_ARGS_DISPLAY).to_string()
    } else {
        let s = args.to_string();
        truncate(&s, MAX_ARGS_DISPLAY).to_string()
    }
}

/// Extract all text from content blocks, joining with space, truncated.
fn extract_text_from_content(blocks: &[ContentBlock], max_chars: usize) -> String {
    let combined: String = blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    // Collapse newlines for single-line display.
    let one_line = combined.replace('\n', "↵ ");
    truncate(&one_line, max_chars).to_string()
}

/// Build a compact result summary: char count + preview of content.
///
/// The char count reflects the full (pre-truncation) combined text so the user
/// can gauge the actual result size.  The preview is then truncated to
/// `max_chars` for display.
fn summarize_result(blocks: &[ContentBlock], max_chars: usize) -> String {
    // Collect the full text first (no truncation) to get the accurate count.
    let full_text = extract_text_from_content(blocks, usize::MAX);
    if full_text.is_empty() {
        return "(no output)".to_string();
    }
    let char_count = full_text.chars().count();
    let preview = truncate(&full_text, max_chars);
    if char_count <= max_chars {
        preview.to_string()
    } else {
        format!("[{char_count} chars] {preview}…")
    }
}

/// Truncate `s` to at most `max` chars (Unicode-aware). No allocation if fits.
fn truncate(s: &str, max: usize) -> &str {
    if s.chars().count() <= max {
        return s;
    }
    // Find the byte position of the `max`-th char boundary.
    match s.char_indices().nth(max) {
        Some((byte_pos, _)) => &s[..byte_pos],
        None => s,
    }
}

/// Stream events from a one-shot run to stdout/stderr with Markdown rendering.
///
/// Spawn this as a tokio task BEFORE calling `harness.prompt()`.
pub async fn render_one_shot(mut rx: broadcast::Receiver<Arc<AgentHarnessEvent>>) {
    let mut renderer = StreamingRenderer::new();

    loop {
        match rx.recv().await {
            Ok(ev) => match ev.as_ref() {
                AgentHarnessEvent::Agent(AgentEvent::TextDelta { text, .. }) => {
                    renderer.push(text);
                }
                AgentHarnessEvent::ToolCallStart {
                    tool_name, args, ..
                } => {
                    // Flush any in-progress raw text before the tool annotation.
                    renderer.flush_raw();
                    print_tool_start(tool_name, args);
                }
                AgentHarnessEvent::ToolCallEnd {
                    tool_name, result, ..
                } => {
                    print_tool_end(tool_name, result);
                }
                AgentHarnessEvent::Agent(AgentEvent::RetryAttempt {
                    attempt,
                    max_retries,
                    delay_ms,
                    error,
                }) => {
                    renderer.flush_raw();
                    print_retry(*attempt, *max_retries, *delay_ms, error);
                }
                AgentHarnessEvent::Agent(AgentEvent::Error(e)) => {
                    eprintln!("\nAgent error: {e}");
                }
                AgentHarnessEvent::PhaseChange {
                    to: HarnessPhase::Idle,
                    ..
                } => {
                    renderer.flush_all();
                    break;
                }
                _ => {}
            },
            Err(broadcast::error::RecvError::Closed) => {
                renderer.flush_all();
                break;
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Tool annotation helpers ───────────────────────────────────────────────

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string_at_char_boundary() {
        let s = "abcdefghij";
        assert_eq!(truncate(s, 5), "abcde");
    }

    #[test]
    fn truncate_unicode_boundary() {
        // Each char is 3 bytes in UTF-8
        let s = "你好世界测试";
        let t = truncate(s, 3);
        assert_eq!(t, "你好世");
    }

    #[test]
    fn format_args_compact_simple_object() {
        let args = serde_json::json!({ "path": "/tmp/foo.rs", "offset": 10 });
        let s = format_args_compact(&args);
        assert!(s.contains("path="), "should contain key: {s}");
        assert!(s.len() <= MAX_ARGS_DISPLAY + 10); // allow small overshoot from truncation
    }

    #[test]
    fn format_args_compact_long_value_truncated() {
        let long_val = "x".repeat(200);
        let args = serde_json::json!({ "content": long_val });
        let s = format_args_compact(&args);
        assert!(s.len() <= MAX_ARGS_DISPLAY + 10);
    }

    #[test]
    fn extract_text_from_content_joins_blocks() {
        let blocks = vec![
            ContentBlock::Text {
                text: "hello".into(),
            },
            ContentBlock::Text {
                text: "world".into(),
            },
        ];
        let result = extract_text_from_content(&blocks, 200);
        assert!(result.contains("hello"));
        assert!(result.contains("world"));
    }

    #[test]
    fn summarize_result_short_fits_inline() {
        let blocks = vec![ContentBlock::Text {
            text: "ok done".into(),
        }];
        let s = summarize_result(&blocks, 200);
        assert_eq!(s, "ok done");
        assert!(!s.contains("["), "short result should not show char count");
    }

    #[test]
    fn summarize_result_long_shows_char_count() {
        let long = "a".repeat(300);
        let blocks = vec![ContentBlock::Text { text: long }];
        let s = summarize_result(&blocks, 100);
        // char count reflects the full 300-char content, not the truncated preview.
        assert!(s.starts_with("[300 chars]"), "got: {s}");
    }

    #[test]
    fn summarize_result_empty_blocks() {
        let s = summarize_result(&[], 100);
        assert_eq!(s, "(no output)");
    }
}
