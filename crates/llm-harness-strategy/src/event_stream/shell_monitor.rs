//! Shell process monitor event stream — monitors a long-running command's
//! stdout and emits batched output as events.
//!
//! Implements item 39 (后台任务监控 EventStream 默认实现).

use std::io;
use std::path::Path;
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio_util::sync::CancellationToken;

use super::template::EventDetails;
use llm_harness_types::{DataBlock, Event, EventStream};

// ── Config ──────────────────────────────────────────────────────────────

/// Batch policy: accumulate stdout lines, emit an Event when a threshold
/// is reached.
#[derive(Debug, Clone)]
pub struct ShellMonitorConfig {
    /// Max accumulated lines before flushing. Default 50.
    max_lines: usize,
    /// Max accumulated time before flushing. Default 5s.
    max_interval: Duration,
}

impl Default for ShellMonitorConfig {
    fn default() -> Self {
        Self {
            max_lines: 50,
            max_interval: Duration::from_secs(5),
        }
    }
}

impl ShellMonitorConfig {
    pub fn with_max_lines(mut self, n: usize) -> Self {
        self.max_lines = n;
        self
    }

    pub fn with_max_interval(mut self, d: Duration) -> Self {
        self.max_interval = d;
        self
    }
}

// ── Handle ──────────────────────────────────────────────────────────────

/// Lightweight control handle for terminating the monitored process.
///
/// Holds a `CancellationToken` — the stream owns the `Child` exclusively.
/// `kill()` is a cancellation request; the stream handles the actual
/// `child.kill().await` + `child.wait().await` in its `next()` select loop.
#[derive(Clone)]
pub struct ShellMonitorHandle {
    kill_token: CancellationToken,
}

impl ShellMonitorHandle {
    /// Request process termination. Idempotent — safe to call multiple
    /// times or after the process has already exited.
    pub fn kill(&self) {
        self.kill_token.cancel();
    }
}

// ── Phase state machine ─────────────────────────────────────────────────

/// Exit-flow state machine.
/// Reading → WaitForExit → Done
enum Phase {
    /// Normal stdout reading.
    Reading,
    /// stdout EOF, buffer drained, need child.wait() for exit_code.
    WaitForExit,
    /// Exit event sent, stream ended.
    Done,
}

// ── Stream ──────────────────────────────────────────────────────────────

/// Shell process monitor event stream.
///
/// Spawns a child process via `sh -c`, reads stdout line-by-line, and
/// emits batched output events. When the process exits, emits a final
/// exit event (with exit_code) then returns `None`.
///
/// See design spec for the pipe-blocking limitation — do not use for
/// high-frequency output commands.
pub struct ShellMonitorStream {
    reader: tokio::io::Lines<BufReader<ChildStdout>>,
    child: Child,
    config: ShellMonitorConfig,
    label: String,
    kill_token: CancellationToken,
    buffer: Vec<String>,
    /// Interval timer for max_interval flush — persists across next() calls
    /// so the time budget is continuous, not reset per call.
    interval: tokio::time::Interval,
    phase: Phase,
}

impl ShellMonitorStream {
    /// Create a monitor stream. Returns `(handle, boxed stream)`.
    ///
    /// - `command`: shell command string (passed to `sh -c`)
    /// - `cwd`: optional working directory override
    /// - `env`: additional environment variables
    /// - `label`: human-readable label for the event source
    /// - `config`: batch thresholds
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        command: &str,
        cwd: Option<&Path>,
        env: Vec<(String, String)>,
        label: impl Into<String>,
        config: ShellMonitorConfig,
    ) -> io::Result<(ShellMonitorHandle, Box<dyn EventStream>)> {
        let kill_token = CancellationToken::new();
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::null());
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &env {
            cmd.env(k, v);
        }
        cmd.kill_on_drop(true);

        let mut child = cmd.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("failed to capture child stdout"))?;
        let reader = BufReader::new(stdout).lines();

        let handle = ShellMonitorHandle {
            kill_token: kill_token.clone(),
        };
        let max_interval = config.max_interval;
        let max_lines_cap = config.max_lines;
        let stream = ShellMonitorStream {
            reader,
            child,
            config,
            label: label.into(),
            kill_token,
            buffer: Vec::with_capacity(max_lines_cap),
            interval: tokio::time::interval(max_interval),
            phase: Phase::Reading,
        };
        Ok((handle, Box::new(stream)))
    }
}

impl ShellMonitorStream {
    /// Build a batched output event from the current buffer.
    fn make_output_event(&self) -> Event {
        let line_count = self.buffer.len() as u32;
        let joined = self.buffer.join("\n");
        let details = EventDetails::new("shell_monitor", "shell_monitor.output")
            .with_label(&self.label)
            .to_value();
        let details = merge_extra(details, &[("line_count", Value::from(line_count))]);
        Event {
            content: vec![DataBlock::text(joined)],
            details,
        }
    }

    /// Build a batched output event with an error annotation.
    fn make_error_output_event(&self, error: &str) -> Event {
        let line_count = self.buffer.len() as u32;
        let joined = self.buffer.join("\n");
        let details = EventDetails::new("shell_monitor", "shell_monitor.output")
            .with_label(&self.label)
            .to_value();
        let details = merge_extra(
            details,
            &[
                ("line_count", Value::from(line_count)),
                ("error", Value::from(error)),
            ],
        );
        Event {
            content: vec![DataBlock::text(joined)],
            details,
        }
    }

    /// Build the exit event with the given exit code.
    ///
    /// On Unix, signal-terminated processes are encoded as negative values
    /// (e.g. -9 for SIGKILL), following shell convention.
    fn make_exit_event(&self, exit_code: i32) -> Event {
        let details = EventDetails::new("shell_monitor", "shell_monitor.exit")
            .with_label(&self.label)
            .to_value();
        let details = merge_extra(details, &[("exit_code", Value::from(exit_code))]);
        let msg = if exit_code < 0 {
            format!("process terminated by signal {}", exit_code.abs())
        } else {
            format!("process exited with code {}", exit_code)
        };
        Event {
            content: vec![DataBlock::text(msg)],
            details,
        }
    }

    /// Wait for child exit, cancellable via kill_token.
    ///
    /// This is the single place `child.wait()` is called. If kill_token
    /// fires (process closed stdout but didn't exit), sends SIGKILL then
    /// waits for the kill to take effect.
    async fn wait_for_exit(&mut self) -> i32 {
        tokio::select! {
            status = self.child.wait() => {
                status.map(exit_status_to_code).unwrap_or(-1)
            }
            _ = self.kill_token.cancelled() => {
                let _ = self.child.kill().await;
                self.child
                    .wait()
                    .await
                    .map(exit_status_to_code)
                    .unwrap_or(-1)
            }
        }
    }
}

impl EventStream for ShellMonitorStream {
    fn next<'a>(&'a mut self) -> BoxFuture<'a, Option<Event>> {
        Box::pin(async move {
            // Handle non-Reading phases first.
            if let Phase::Done = self.phase {
                return None;
            }
            if let Phase::WaitForExit = self.phase {
                // Unified exit handling — both EOF and kill arrive here.
                // Wrap wait() in select! so kill_token can still interrupt
                // if the process closes stdout but doesn't exit (e.g. daemon
                // that forked a surviving child).
                let exit_code = self.wait_for_exit().await;
                self.phase = Phase::Done;
                return Some(self.make_exit_event(exit_code));
            }

            // Phase::Reading
            loop {
                tokio::select! {
                    // Kill request — flush buffer first (data loss prevention),
                    // then send SIGKILL and fall through to WaitForExit.
                    _ = self.kill_token.cancelled() => {
                        if !self.buffer.is_empty() {
                            let event = self.make_output_event();
                            self.buffer.clear();
                            self.phase = Phase::WaitForExit;
                            return Some(event);
                        }
                        let _ = self.child.kill().await;
                        self.phase = Phase::WaitForExit;
                        break;
                    }
                    // Read next line from stdout
                    result = self.reader.next_line() => {
                        match result {
                            Ok(Some(line)) => {
                                self.buffer.push(line);
                                if self.buffer.len() >= self.config.max_lines {
                                    let event = self.make_output_event();
                                    self.buffer.clear();
                                    return Some(event);
                                }
                            }
                            Ok(None) => {
                                // EOF
                                if !self.buffer.is_empty() {
                                    let event = self.make_output_event();
                                    self.buffer.clear();
                                    self.phase = Phase::WaitForExit;
                                    return Some(event);
                                }
                                // Buffer empty — fall through to WaitForExit
                                self.phase = Phase::WaitForExit;
                                break;
                            }
                            Err(e) => {
                                // Read error — emit with error annotation, then exit flow
                                let event = self.make_error_output_event(&e.to_string());
                                self.buffer.clear();
                                self.phase = Phase::WaitForExit;
                                return Some(event);
                            }
                        }
                    }
                    // Interval timeout — flush if buffer non-empty
                    _ = self.interval.tick() => {
                        if !self.buffer.is_empty() {
                            let event = self.make_output_event();
                            self.buffer.clear();
                            return Some(event);
                        }
                        // Buffer empty — continue looping
                    }
                }
            }

            // Fall-through from Reading: unified WaitForExit handling.
            let exit_code = self.wait_for_exit().await;
            self.phase = Phase::Done;
            Some(self.make_exit_event(exit_code))
        })
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Merge EventDetails JSON with extra fields.
///
/// # Precondition
/// `details` must be a JSON Object (as produced by `EventDetails::to_value()`).
/// Non-object values are silently replaced with an empty map.
fn merge_extra(mut details: Value, extra: &[(&str, Value)]) -> Value {
    if let Some(map) = details.as_object_mut() {
        for (k, v) in extra {
            map.insert((*k).to_string(), v.clone());
        }
    }
    details
}

/// Convert `ExitStatus` to an exit code, preserving signal information.
///
/// On Unix, signal-terminated processes return negative values (e.g. -9
/// for SIGKILL), following shell convention. On non-Unix, falls back to
/// `-1`.
fn exit_status_to_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(0))
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(-1)
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default() {
        let c = ShellMonitorConfig::default();
        assert_eq!(c.max_lines, 50);
        assert_eq!(c.max_interval, Duration::from_secs(5));
    }

    #[test]
    fn config_builder() {
        let c = ShellMonitorConfig::default()
            .with_max_lines(10)
            .with_max_interval(Duration::from_millis(500));
        assert_eq!(c.max_lines, 10);
        assert_eq!(c.max_interval, Duration::from_millis(500));
    }

    #[tokio::test]
    async fn handle_is_clone_and_send() {
        let (handle, _stream) = ShellMonitorStream::new(
            "sleep 60",
            None,
            vec![],
            "test",
            ShellMonitorConfig::default(),
        )
        .unwrap();
        let h2 = handle.clone();
        let join = tokio::spawn(async move {
            h2.kill();
        });
        join.await.unwrap();
        handle.kill();
    }

    #[tokio::test]
    async fn kill_is_idempotent() {
        let (handle, _stream) = ShellMonitorStream::new(
            "sleep 60",
            None,
            vec![],
            "test",
            ShellMonitorConfig::default(),
        )
        .unwrap();
        handle.kill();
        handle.kill();
        handle.kill();
    }

    #[tokio::test]
    async fn basic_output() {
        let (_handle, mut stream) = ShellMonitorStream::new(
            r#"echo "line1"; echo "line2""#,
            None,
            vec![],
            "test",
            ShellMonitorConfig::default(),
        )
        .unwrap();

        // First next() → output event with both lines
        let e1 = stream.next().await.unwrap();
        let text = text_of_event(&e1);
        assert!(text.contains("line1"), "got: {text}");
        assert!(text.contains("line2"), "got: {text}");

        // Second next() → exit event
        let e2 = stream.next().await.unwrap();
        let exit_text = text_of_event(&e2);
        assert!(exit_text.contains("process exited"), "got: {exit_text}");

        // Third next() → None
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn process_exit_returns_none() {
        let (_handle, mut stream) = ShellMonitorStream::new(
            "echo done",
            None,
            vec![],
            "test",
            ShellMonitorConfig::default(),
        )
        .unwrap();

        // Output event
        let _e1 = stream.next().await.unwrap();
        // Exit event
        let _e2 = stream.next().await.unwrap();
        // None
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn exit_code_in_details() {
        let (_handle, mut stream) = ShellMonitorStream::new(
            "exit 3",
            None,
            vec![],
            "test",
            ShellMonitorConfig::default(),
        )
        .unwrap();

        // No stdout output → first next() returns exit event directly
        let e = stream.next().await.unwrap();
        assert_eq!(e.details["channel"], "shell_monitor.exit");
        assert_eq!(e.details["exit_code"], 3);
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn event_details_source_channel() {
        let (_handle, mut stream) = ShellMonitorStream::new(
            "echo hello",
            None,
            vec![],
            "test",
            ShellMonitorConfig::default(),
        )
        .unwrap();

        let e = stream.next().await.unwrap();
        assert_eq!(e.details["source"], "shell_monitor");
        assert_eq!(e.details["channel"], "shell_monitor.output");
    }

    #[tokio::test]
    async fn exit_event_details() {
        let (_handle, mut stream) = ShellMonitorStream::new(
            "echo hi",
            None,
            vec![],
            "test",
            ShellMonitorConfig::default(),
        )
        .unwrap();

        let _ = stream.next().await.unwrap(); // output
        let exit = stream.next().await.unwrap(); // exit
        assert_eq!(exit.details["source"], "shell_monitor");
        assert_eq!(exit.details["channel"], "shell_monitor.exit");
    }

    #[tokio::test]
    async fn stderr_not_monitored() {
        let (_handle, mut stream) = ShellMonitorStream::new(
            r#"echo "err msg" >&2; echo "out msg""#,
            None,
            vec![],
            "test",
            ShellMonitorConfig::default(),
        )
        .unwrap();

        let e = stream.next().await.unwrap();
        let text = text_of_event(&e);
        assert!(text.contains("out msg"), "got: {text}");
        assert!(
            !text.contains("err msg"),
            "stderr should not appear: {text}"
        );
    }

    /// Helper: extract text from the first DataBlock of an event.
    fn text_of_event(e: &Event) -> String {
        match &e.content[0] {
            llm_harness_types::DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text block"),
        }
    }

    #[tokio::test]
    async fn batch_by_line_count() {
        // Output 100 lines, max_lines=50 → first event has 50 lines,
        // second event has 50 lines, then exit event.
        let (_handle, mut stream) = ShellMonitorStream::new(
            "for i in $(seq 1 100); do echo \"line $i\"; done",
            None,
            vec![],
            "batch-test",
            ShellMonitorConfig::default().with_max_lines(50),
        )
        .unwrap();

        let e1 = stream.next().await.unwrap();
        assert_eq!(e1.details["line_count"], 50);
        let text1 = text_of_event(&e1);
        assert!(text1.contains("line 1"), "got: {text1}");
        assert!(text1.contains("line 50"), "got: {text1}");

        let e2 = stream.next().await.unwrap();
        assert_eq!(e2.details["line_count"], 50);
        let text2 = text_of_event(&e2);
        assert!(text2.contains("line 51"), "got: {text2}");
        assert!(text2.contains("line 100"), "got: {text2}");

        let e3 = stream.next().await.unwrap();
        assert_eq!(e3.details["channel"], "shell_monitor.exit");

        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn batch_by_interval() {
        // sleep 0.2s then echo — with max_interval=100ms, the timeout
        // fires first with empty buffer (continues), then line arrives.
        // Use a command that outputs slowly: echo with sleeps between lines.
        let (_handle, mut stream) = ShellMonitorStream::new(
            r#"echo "first"; sleep 0.3; echo "second""#,
            None,
            vec![],
            "interval-test",
            ShellMonitorConfig::default()
                .with_max_lines(100) // high so line count doesn't trigger
                .with_max_interval(Duration::from_millis(100)),
        )
        .unwrap();

        // First: "first" line arrives, then interval fires with buffer non-empty
        // Actually "first" is pushed to buffer, then interval tick fires → flush.
        let e1 = stream.next().await.unwrap();
        let text1 = text_of_event(&e1);
        assert!(text1.contains("first"), "got: {text1}");

        // Then "second" arrives, either flushed by interval or by EOF
        let e2 = stream.next().await.unwrap();
        let text2 = text_of_event(&e2);
        assert!(text2.contains("second"), "got: {text2}");

        // Exit event
        let e3 = stream.next().await.unwrap();
        assert_eq!(e3.details["channel"], "shell_monitor.exit");

        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn kill_terminates_process() {
        let (handle, mut stream) = ShellMonitorStream::new(
            "sleep 60",
            None,
            vec![],
            "kill-test",
            ShellMonitorConfig::default(),
        )
        .unwrap();

        // Give the process time to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        handle.kill();

        // Should get exit event (not output event, since no stdout)
        let e = stream.next().await.unwrap();
        assert_eq!(e.details["channel"], "shell_monitor.exit");
        // Killed process typically has non-zero exit code
        let exit_code = e.details["exit_code"].as_i64().unwrap();
        assert!(
            exit_code != 0,
            "killed process should have non-zero exit code, got {exit_code}"
        );

        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn empty_output_exits_directly() {
        // Command produces no stdout → first next() returns exit event
        let (_handle, mut stream) = ShellMonitorStream::new(
            "true",
            None,
            vec![],
            "empty-test",
            ShellMonitorConfig::default(),
        )
        .unwrap();

        let e = stream.next().await.unwrap();
        assert_eq!(e.details["channel"], "shell_monitor.exit");
        assert_eq!(e.details["exit_code"], 0);
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn kill_flushes_buffered_output() {
        // Command emits lines slowly, kill mid-stream → buffered lines must
        // be flushed as a final output event before the exit event.
        let (handle, mut stream) = ShellMonitorStream::new(
            r#"for i in $(seq 1 100); do echo "line $i"; sleep 0.05; done"#,
            None,
            vec![],
            "kill-flush-test",
            ShellMonitorConfig::default()
                .with_max_lines(1000) // high so no line-count flush
                .with_max_interval(Duration::from_secs(10)),
        )
        .unwrap();

        // Concurrently: wait for lines to accumulate, then kill.
        // next() is blocked in the Reading select! loop when kill fires.
        let kill_handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            handle.kill();
        });

        // First event: flushed buffered output (not exit)
        let e1 = stream.next().await.unwrap();
        kill_handle.await.unwrap();
        assert_eq!(
            e1.details["channel"], "shell_monitor.output",
            "expected output event from buffer flush, got: {:?}",
            e1.details
        );
        let text1 = text_of_event(&e1);
        assert!(
            text1.contains("line 1"),
            "buffered output should contain line 1: {text1}"
        );

        // Second event: exit
        let e2 = stream.next().await.unwrap();
        assert_eq!(e2.details["channel"], "shell_monitor.exit");

        // Stream ends
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn kill_in_wait_for_exit_phase() {
        // Process closes stdout but doesn't exit (simulates daemon fork).
        // After EOF, stream enters WaitForExit. kill() must interrupt
        // the blocking wait().
        let (handle, mut stream) = ShellMonitorStream::new(
            // Close stdout, then sleep forever
            r#"exec sleep 60 >&-"#,
            None,
            vec![],
            "wait-exit-kill-test",
            ShellMonitorConfig::default(),
        )
        .unwrap();

        // Give the process time to start and close stdout
        tokio::time::sleep(Duration::from_millis(200)).await;

        // First next() returns None (EOF, empty buffer → WaitForExit fallthrough)
        // Actually: EOF with empty buffer → WaitForExit → wait_for_exit blocks
        // We need to call next() in a task so we can kill concurrently.
        let next_task = tokio::spawn(async move { stream.next().await });

        // Give next() time to reach the blocking wait()
        tokio::time::sleep(Duration::from_millis(200)).await;

        handle.kill();

        // next() should now complete with an exit event
        let e = next_task.await.unwrap().unwrap();
        assert_eq!(e.details["channel"], "shell_monitor.exit");
    }

    #[tokio::test]
    async fn output_event_has_no_exit_code() {
        let (_handle, mut stream) = ShellMonitorStream::new(
            "echo hello",
            None,
            vec![],
            "no-null-test",
            ShellMonitorConfig::default(),
        )
        .unwrap();

        let e = stream.next().await.unwrap();
        assert_eq!(e.details["channel"], "shell_monitor.output");
        assert!(
            e.details.get("exit_code").is_none(),
            "output event should not have exit_code field"
        );
    }
}
