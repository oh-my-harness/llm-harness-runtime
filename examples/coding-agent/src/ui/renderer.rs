//! Streaming Markdown renderer for incremental text deltas.
//!
//! Accumulates incoming `TextDelta`s and renders complete blocks
//! (paragraphs and code blocks) via `termimad`, while printing
//! in-progress text raw for a "typewriter" feel.
//!
//! # Strategy
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

use std::io::Write;

use termimad::MadSkin;

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
pub(crate) fn count_open_fences(text: &str) -> usize {
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
pub(crate) fn find_paragraph_boundary(text: &str) -> Option<usize> {
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
