//! Event-to-terminal rendering for coding-agent.
//!
//! Subscribes to `AgentHarnessEvent` broadcast and renders:
//! - `TextDelta` → streaming markdown render (see [`StreamingRenderer`])
//! - `ToolCallStart` → "🔧 tool_name { args }" on a dedicated line
//! - `ToolCallEnd` → "✓ <summary>" or "✗ <error>" on a dedicated line
//! - `RetryAttempt` → "⟳ retry N/M in Xs: <reason>" warning line
//! - `Error` → stderr
//! - `PhaseChange { to: Idle }` → flush remaining buffer and break
//!
//! # Streaming Markdown Strategy
//!
//! We can't render Markdown correctly mid-stream (e.g. an unclosed code fence
//! looks like plain text). Instead we use a **block-boundary flush** approach:
//!
//! - Accumulate incoming `TextDelta`s in a buffer.
//! - When a **paragraph boundary** (`\n\n`) is detected outside a code fence,
//!   flush and render that paragraph via `termimad`.
//! - When a **code fence** closes (paired ` ``` `), flush and render the whole
//!   fenced block via `termimad`.
//! - **Within** an in-progress block, print raw text so the user sees output
//!   immediately (the "typewriter" feel).
//! - On `PhaseChange → Idle`, flush whatever remains in the buffer.
//!
//! # Tool Call Rendering
//!
//! Tool call lines are printed outside the `StreamingRenderer` buffer so they
//! do not interfere with the Markdown stream. Before printing a tool line we
//! flush any raw-printed characters so the display stays coherent.

use std::io::Write;
use std::sync::Arc;

use llm_harness_agent::{AgentHarnessEvent, HarnessToolCallResult};
use llm_harness_types::{AgentEvent, ContentBlock, HarnessPhase};
use termimad::MadSkin;
use tokio::sync::broadcast;

// ── ANSI color helpers ────────────────────────────────────────────────────────

/// ANSI escape codes used for tool-call annotation lines.
/// We only write these when stdout looks like a terminal; for simplicity we
/// always emit them (the user can pipe through `cat` to strip if needed).
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

// ── StreamingRenderer ─────────────────────────────────────────────────────────

/// Stateful renderer that accepts incremental text deltas and emits
/// formatted Markdown to stdout whenever a complete block is available.
pub struct StreamingRenderer {
    skin: MadSkin,
    /// Accumulated text not yet rendered.
    buf: String,
    /// How many ``` fences are currently open (0 = outside code block).
    fence_depth: usize,
    /// Characters printed raw (before the next rendered flush) that need
    /// to be cleared before re-printing the rendered version.
    raw_chars_printed: usize,
}

impl StreamingRenderer {
    pub fn new() -> Self {
        Self {
            skin: MadSkin::default(),
            buf: String::new(),
            fence_depth: 0,
            raw_chars_printed: 0,
        }
    }

    /// Feed a new delta fragment. Renders completed blocks immediately.
    pub fn push(&mut self, delta: &str) {
        for ch in delta.chars() {
            self.buf.push(ch);

            // Track code-fence depth on newlines
            if ch == '\n' {
                self.update_fence_depth();
            }
        }

        self.try_flush_blocks();
    }

    /// Flush all remaining buffered content (called at end of stream).
    pub fn flush_all(&mut self) {
        if !self.buf.is_empty() {
            self.clear_raw();
            let content = std::mem::take(&mut self.buf);
            self.render_block(&content);
            self.raw_chars_printed = 0;
        }
        let _ = std::io::stdout().flush();
    }

    /// Clear any raw-printed characters so an out-of-band line (e.g. a tool
    /// annotation) can be printed without overlapping buffered text.
    ///
    /// The buffered content is NOT discarded; the next `push()` call will
    /// raw-print it again from the beginning.
    pub fn flush_raw(&mut self) {
        if self.raw_chars_printed > 0 {
            self.clear_raw();
            // raw_chars_printed is reset to 0 inside clear_raw.
        }
    }

    // ── private ───────────────────────────────────────────────────────────────

    /// Recount fence depth by scanning the whole buffer.
    /// Called after each newline so we always have an accurate count.
    fn update_fence_depth(&mut self) {
        self.fence_depth = count_open_fences(&self.buf);
    }

    /// Try to extract and render complete blocks from the front of the buffer.
    fn try_flush_blocks(&mut self) {
        loop {
            if self.fence_depth > 0 {
                // Inside a code block — nothing to flush yet, just raw-print
                // any new chars so the user sees progress.
                self.raw_print_tail();
                break;
            }

            // Look for a paragraph boundary (\n\n) in the buffer.
            if let Some(pos) = find_paragraph_boundary(&self.buf) {
                let block: String = self.buf.drain(..pos).collect();
                // Remove the leading \n\n separator that follows the block.
                if self.buf.starts_with("\n\n") {
                    self.buf.drain(..2);
                }
                self.clear_raw();
                self.render_block(block.trim_end_matches('\n'));
                self.raw_chars_printed = 0;
                // Re-sync fence depth for the remaining buffer.
                self.fence_depth = count_open_fences(&self.buf);
            } else {
                // No complete paragraph yet — raw-print the tail for immediacy.
                self.raw_print_tail();
                break;
            }
        }
    }

    /// Print any new characters added since last raw print.
    fn raw_print_tail(&mut self) {
        let already = self.raw_chars_printed;
        let total: usize = self.buf.chars().count();
        if total > already {
            let new_part: String = self.buf.chars().skip(already).collect();
            print!("{new_part}");
            let _ = std::io::stdout().flush();
            self.raw_chars_printed = total;
        }
    }

    /// Clear the raw-printed characters by overwriting with spaces, then move
    /// cursor back. This lets termimad re-render the block cleanly.
    fn clear_raw(&mut self) {
        if self.raw_chars_printed == 0 {
            return;
        }
        // Count how many actual terminal lines the raw text occupied.
        let raw_text: String = self.buf.chars().take(self.raw_chars_printed).collect();
        let newline_count = raw_text.chars().filter(|&c| c == '\n').count();

        // Move up `newline_count` lines, then clear from cursor to end.
        if newline_count > 0 {
            // Move up N lines
            print!("\x1b[{newline_count}A");
        }
        // Clear from cursor to end of screen
        print!("\x1b[J");
        let _ = std::io::stdout().flush();
        self.raw_chars_printed = 0;
    }

    /// Render a complete block via termimad and print a trailing newline.
    fn render_block(&self, block: &str) {
        if block.is_empty() {
            return;
        }
        self.skin.print_text(block);
        // termimad's print_text doesn't always append a trailing newline when
        // the block ends without one, so we ensure separation.
        println!();
    }
}

impl Default for StreamingRenderer {
    fn default() -> Self {
        Self::new()
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Count how many code fences are currently open in `text`.
/// A fence is a line that starts with ``` (optionally preceded by spaces).
/// Each ``` line toggles the open/closed state.
fn count_open_fences(text: &str) -> usize {
    let mut depth = 0usize;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            depth = depth.saturating_add(1) % 2;
        }
    }
    // If the buffer ends mid-line (no trailing newline after the fence marker),
    // lines() still includes the last incomplete line, so no extra handling needed.
    depth
}

/// Find the byte position of the first paragraph boundary (`\n\n`) that
/// occurs **outside** a code fence. Returns the position of the first `\n`.
fn find_paragraph_boundary(text: &str) -> Option<usize> {
    // Scan line by line, track fence state, find the first empty line outside a fence.
    let mut fence_open = false;
    let mut line_start_byte = 0usize;

    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            fence_open = !fence_open;
        }

        if !fence_open && (line == "\n" || line.trim().is_empty()) && line_start_byte > 0 {
            // This is an empty line outside a fence — paragraph boundary.
            // Return the byte position of the start of this empty line.
            return Some(line_start_byte);
        }

        line_start_byte += line.len();
    }
    None
}

// ── public render entry point ─────────────────────────────────────────────────

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

/// Build a compact result summary: char count + first line of content.
fn summarize_result(blocks: &[ContentBlock], max_chars: usize) -> String {
    let text = extract_text_from_content(blocks, max_chars * 2);
    if text.is_empty() {
        return "(no output)".to_string();
    }
    // Show char count so user can gauge result size, then a preview.
    let char_count = text.chars().count();
    let preview = truncate(&text, max_chars);
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

    #[test]
    fn count_open_fences_zero_for_plain_text() {
        assert_eq!(count_open_fences("hello\nworld\n"), 0);
    }

    #[test]
    fn count_open_fences_one_for_open_fence() {
        let text = "before\n```rust\nlet x = 1;\n";
        assert_eq!(count_open_fences(text), 1);
    }

    #[test]
    fn count_open_fences_zero_for_closed_fence() {
        let text = "before\n```rust\nlet x = 1;\n```\nafter\n";
        assert_eq!(count_open_fences(text), 0);
    }

    #[test]
    fn find_paragraph_boundary_returns_none_for_no_blank_line() {
        let text = "line one\nline two\n";
        assert_eq!(find_paragraph_boundary(text), None);
    }

    #[test]
    fn find_paragraph_boundary_finds_first_blank_line() {
        let text = "para one\n\npara two\n";
        let pos = find_paragraph_boundary(text).expect("should find boundary");
        // The boundary is after "para one\n", so byte pos = 9
        assert_eq!(&text[..pos], "para one\n");
    }

    #[test]
    fn find_paragraph_boundary_ignores_blank_line_inside_fence() {
        let text = "```\ncode\n\nmore code\n```\n\nafter\n";
        let pos = find_paragraph_boundary(text).expect("should find boundary after fence");
        // Blank line inside fence is skipped; boundary is after the closing fence
        assert!(
            &text[..pos].contains("```"),
            "boundary should be after the fence, got prefix: {:?}",
            &text[..pos]
        );
    }

    #[test]
    fn renderer_push_does_not_panic_on_plain_text() {
        let mut r = StreamingRenderer::new();
        r.push("Hello, world!\n\nSecond paragraph.\n");
        r.flush_all();
    }

    #[test]
    fn renderer_push_does_not_panic_on_code_block() {
        let mut r = StreamingRenderer::new();
        r.push("Some text\n\n```rust\nfn main() {}\n```\n\nAfter.\n");
        r.flush_all();
    }

    #[test]
    fn renderer_handles_incremental_deltas() {
        let mut r = StreamingRenderer::new();
        // Simulate streaming: deliver text character by character
        for ch in "Hello **world**\n\nNext para\n".chars() {
            r.push(&ch.to_string());
        }
        r.flush_all();
    }

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
        // extract_text_from_content caps at max_chars*2=200, so char count is 200
        assert!(s.starts_with("[200 chars]"), "got: {s}");
    }

    #[test]
    fn summarize_result_empty_blocks() {
        let s = summarize_result(&[], 100);
        assert_eq!(s, "(no output)");
    }

    #[test]
    fn flush_raw_resets_printed_count() {
        let mut r = StreamingRenderer::new();
        // Push some text that goes to raw buffer (no paragraph boundary yet)
        r.push("partial line without newline");
        // flush_raw should clear the raw counter without discarding the buffer
        r.flush_raw();
        assert_eq!(r.raw_chars_printed, 0);
        // Buffer still has content; flush_all should not panic
        r.flush_all();
    }
}
