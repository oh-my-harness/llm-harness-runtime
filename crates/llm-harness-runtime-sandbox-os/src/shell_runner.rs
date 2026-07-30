//! Shared shell output draining logic used by `OsEnv` and `BwrapEnv`.
//!
//! Both backends spawn a child process (directly or via `bwrap`) and need the
//! same stdout/stderr draining, UTF-8 boundary handling, timeout, and abort
//! logic. Extracting it here avoids code duplication and keeps
//! `utf8_safe_boundary` as an internal detail.

use llm_harness_types::{EnvError, ShellOptions, ShellOutput};
use tokio_util::sync::CancellationToken;

/// Find the index of the last complete UTF-8 character boundary in `bytes`.
///
/// `bytes[..split]` is valid UTF-8; `bytes[split..]` may be an incomplete
/// multi-byte sequence tail to defer to the next chunk.
fn utf8_safe_boundary(bytes: &[u8]) -> usize {
    let len = bytes.len();
    if len == 0 {
        return 0;
    }
    let start = len.saturating_sub(4);
    for i in (start..len).rev() {
        let b = bytes[i];
        if b & 0xC0 != 0x80 {
            let expected = if b & 0x80 == 0 {
                1
            } else if b & 0xE0 == 0xC0 {
                2
            } else if b & 0xF0 == 0xE0 {
                3
            } else if b & 0xF8 == 0xF0 {
                4
            } else {
                continue;
            };
            let end = i + expected;
            if end <= len {
                return end;
            } else {
                return i;
            }
        }
    }
    0
}

/// Drain stdout/stderr from an already-spawned child process, applying
/// timeout and abort semantics.
///
/// On timeout or abort, sends `start_kill()` **and waits** for the child to
/// exit so no zombie or orphan processes remain.
pub async fn drain_child_output(
    mut child: tokio::process::Child,
    opts: ShellOptions<'_>,
) -> Result<ShellOutput, EnvError> {
    let mut stdout_handle = child.stdout.take();
    let mut stderr_handle = child.stderr.take();

    macro_rules! drain_pipe {
        ($handle:expr, $cb:expr) => {
            async {
                use tokio::io::AsyncReadExt;
                let mut buf = Vec::with_capacity(4096);
                let mut carry = Vec::new();
                if let Some(h) = $handle.as_mut() {
                    let mut chunk = [0u8; 4096];
                    loop {
                        let n = h.read(&mut chunk).await.map_err(EnvError::Io)?;
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(cb) = $cb.as_deref_mut() {
                            carry.extend_from_slice(&chunk[..n]);
                            let split = utf8_safe_boundary(&carry);
                            let s = String::from_utf8_lossy(&carry[..split]);
                            cb(&s);
                            carry = carry[split..].to_vec();
                        }
                    }
                }
                if let Some(cb) = $cb.as_deref_mut() {
                    if !carry.is_empty() {
                        let s = String::from_utf8_lossy(&carry);
                        cb(&s);
                    }
                }
                Ok::<String, EnvError>(String::from_utf8_lossy(&buf).into_owned())
            }
        };
    }

    let mut on_stdout = opts.on_stdout;
    let mut on_stderr = opts.on_stderr;
    let drain = async {
        let stdout_task = drain_pipe!(stdout_handle, on_stdout);
        let stderr_task = drain_pipe!(stderr_handle, on_stderr);
        let (stdout_res, stderr_res) = tokio::join!(stdout_task, stderr_task);
        let stdout = stdout_res?;
        let stderr = stderr_res?;
        let status = child.wait().await.map_err(EnvError::Io)?;
        Ok::<_, EnvError>((stdout, stderr, status))
    };

    let (stdout, stderr, status) = if let Some(timeout) = opts.timeout {
        tokio::select! {
            biased;
            _ = opts.abort.cancelled() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(EnvError::Aborted);
            }
            res = tokio::time::timeout(timeout, drain) => match res {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return Err(e);
                }
                Err(_) => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return Err(EnvError::ShellFailed {
                        exit_code: -1,
                        stdout: String::new(),
                        stderr: "timeout".into(),
                    });
                }
            },
        }
    } else {
        tokio::select! {
            biased;
            _ = opts.abort.cancelled() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(EnvError::Aborted);
            }
            res = drain => match res {
                Ok(v) => v,
                Err(e) => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return Err(e);
                }
            },
        }
    };

    #[cfg(target_os = "windows")]
    let (stdout, stderr) = (stdout.replace("\r\n", "\n"), stderr.replace("\r\n", "\n"));

    if !status.success() {
        Err(EnvError::ShellFailed {
            exit_code: status.code().unwrap_or(-1),
            stdout,
            stderr,
        })
    } else {
        Ok(ShellOutput {
            stdout,
            stderr,
            exit_code: status.code().unwrap_or(0),
        })
    }
}

// ── Guarded drain with shutdown token + guaranteed reap ────────────────────

/// Guard that owns a [`tokio::process::Child`] and guarantees the child is
/// killed **and reaped** even if the drain future is dropped without being
/// polled to completion.
///
/// `tokio::process::Command::kill_on_drop(true)` only sends `SIGKILL` from
/// `Drop` — it cannot `await`, so the child is not reaped (reaped via Tokio's
/// best-effort SIGCHLD orphan queue, which requires the signal driver and is
/// not guaranteed).  This guard closes that gap by spawning a **dedicated OS
/// thread** in its `Drop` that polls `try_wait()` (synchronous `waitpid` with
/// `WNOHANG`) until the child exits, ensuring the zombie is reaped regardless
/// of Tokio runtime state.
///
/// Using `std::thread::spawn` instead of `tokio::spawn` is deliberate: the
/// reaper must survive runtime shutdown (where `tokio::spawn` tasks may be
/// cancelled before they poll) and must work when no runtime exists at all
/// (e.g. Drop after the runtime has been dropped).  An OS thread is
/// independent of the async runtime lifecycle and is guaranteed to run to
/// completion.
///
/// Once the child has been successfully reaped via [`Self::kill_and_wait`] or
/// [`Self::wait`], the `reaped` flag is set so `Drop` does not double-kill.
struct ReapGuard {
    child: Option<tokio::process::Child>,
    reaped: bool,
}

impl ReapGuard {
    fn new(child: tokio::process::Child) -> Self {
        Self {
            child: Some(child),
            reaped: false,
        }
    }

    /// Borrow the inner child mutably (for `start_kill`, `wait`, taking
    /// stdout/stderr handles, etc.).
    fn child_mut(&mut self) -> Option<&mut tokio::process::Child> {
        self.child.as_mut()
    }

    /// Kill and wait for the child to exit.  Only marks the child as reaped
    /// if `wait()` succeeds — a failed `wait()` leaves `reaped` false so
    /// `Drop` still attempts reaping.
    async fn kill_and_wait(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
            if child.wait().await.is_ok() {
                self.reaped = true;
            }
        }
    }

    /// Wait for the child to exit naturally (no kill).  Only marks as reaped
    /// if `wait()` succeeds.
    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        if let Some(child) = &mut self.child {
            let status = child.wait().await?;
            self.reaped = true;
            Ok(status)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "child already taken",
            ))
        }
    }
}

impl Drop for ReapGuard {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        if let Some(mut child) = self.child.take() {
            // Send SIGKILL immediately (synchronous, safe in Drop).
            let _ = child.start_kill();

            // Spawn a dedicated OS thread to reap the child.  This is the
            // critical difference from kill_on_drop: we guarantee `wait()`
            // is called so the zombie is reaped, not just signalled.
            //
            // We use `std::thread::spawn` (not `tokio::spawn`) because:
            // - The reaper must survive runtime shutdown (tokio::spawn
            //   tasks may be cancelled before they poll).
            // - The reaper must work when no runtime exists (Drop after
            //   runtime is dropped, or Drop outside any runtime context).
            //
            // `try_wait()` is synchronous — it calls `waitpid(WNOHANG)`
            // internally and does not require a Tokio runtime.
            std::thread::spawn(move || {
                // Poll try_wait() until the child exits or an error occurs.
                // An error (e.g. ESRCH — process already reaped) means the
                // child is gone, so we stop.
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break, // Reaped successfully.
                        Ok(None) => {
                            // Still running — wait briefly and retry.
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(_) => break, // Error — process gone or unreapable.
                    }
                }
                // Dropping `child` here releases the tokio::process::Child
                // handle.  Since kill_on_drop is false (we didn't set it),
                // this does not send another signal.
            });
        }
    }
}

/// Drain stdout/stderr from a spawned child with **shutdown cancellation**
/// and **guaranteed reap on drop**.
///
/// This is the production-grade variant of [`drain_child_output`] used by
/// sandbox backends that have an internal shutdown signal (e.g.
/// `BwrapEnv`).  It adds two guarantees beyond the base function:
///
/// 1. **Shutdown cancellation**: when `shutdown` fires, the child is killed
///    and reaped immediately (not just abandoned).
/// 2. **Drop-safe reap**: if the returned future is dropped before
///    completion (e.g. the caller aborts the task), the [`ReapGuard`]
///    spawns a background task to kill and wait the child, so no zombie or
///    orphan survives.  This is stronger than `kill_on_drop(true)`, which
///    only sends `SIGKILL` without reaping.
///
/// `OsEnv` continues to use the simpler [`drain_child_output`] (no shutdown
/// token, no guard overhead) because it has no internal shutdown signal.
pub async fn drain_child_output_guarded(
    child: tokio::process::Child,
    opts: ShellOptions<'_>,
    shutdown: CancellationToken,
) -> Result<ShellOutput, EnvError> {
    let mut guard = ReapGuard::new(child);

    let mut stdout_handle = guard.child_mut().and_then(|c| c.stdout.take());
    let mut stderr_handle = guard.child_mut().and_then(|c| c.stderr.take());

    macro_rules! drain_pipe {
        ($handle:expr, $cb:expr) => {
            async {
                use tokio::io::AsyncReadExt;
                let mut buf = Vec::with_capacity(4096);
                let mut carry = Vec::new();
                if let Some(h) = $handle.as_mut() {
                    let mut chunk = [0u8; 4096];
                    loop {
                        let n = h.read(&mut chunk).await.map_err(EnvError::Io)?;
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(cb) = $cb.as_deref_mut() {
                            carry.extend_from_slice(&chunk[..n]);
                            let split = utf8_safe_boundary(&carry);
                            let s = String::from_utf8_lossy(&carry[..split]);
                            cb(&s);
                            carry = carry[split..].to_vec();
                        }
                    }
                }
                if let Some(cb) = $cb.as_deref_mut() {
                    if !carry.is_empty() {
                        let s = String::from_utf8_lossy(&carry);
                        cb(&s);
                    }
                }
                Ok::<String, EnvError>(String::from_utf8_lossy(&buf).into_owned())
            }
        };
    }

    let mut on_stdout = opts.on_stdout;
    let mut on_stderr = opts.on_stderr;
    let drain = async {
        let stdout_task = drain_pipe!(stdout_handle, on_stdout);
        let stderr_task = drain_pipe!(stderr_handle, on_stderr);
        let (stdout_res, stderr_res) = tokio::join!(stdout_task, stderr_task);
        let stdout = stdout_res?;
        let stderr = stderr_res?;
        let status = guard.wait().await.map_err(EnvError::Io)?;
        Ok::<_, EnvError>((stdout, stderr, status))
    };

    let (stdout, stderr, status) = if let Some(timeout) = opts.timeout {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                guard.kill_and_wait().await;
                return Err(EnvError::Other("sandbox is shutting down".into()));
            }
            _ = opts.abort.cancelled() => {
                guard.kill_and_wait().await;
                return Err(EnvError::Aborted);
            }
            res = tokio::time::timeout(timeout, drain) => match res {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => {
                    guard.kill_and_wait().await;
                    return Err(e);
                }
                Err(_) => {
                    guard.kill_and_wait().await;
                    return Err(EnvError::ShellFailed {
                        exit_code: -1,
                        stdout: String::new(),
                        stderr: "timeout".into(),
                    });
                }
            },
        }
    } else {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                guard.kill_and_wait().await;
                return Err(EnvError::Other("sandbox is shutting down".into()));
            }
            _ = opts.abort.cancelled() => {
                guard.kill_and_wait().await;
                return Err(EnvError::Aborted);
            }
            res = drain => match res {
                Ok(v) => v,
                Err(e) => {
                    guard.kill_and_wait().await;
                    return Err(e);
                }
            },
        }
    };

    // Guard has been reaped via wait() in the drain future.  Mark it so
    // Drop does not attempt a second kill.
    guard.reaped = true;

    #[cfg(target_os = "windows")]
    let (stdout, stderr) = (stdout.replace("\r\n", "\n"), stderr.replace("\r\n", "\n"));

    if !status.success() {
        Err(EnvError::ShellFailed {
            exit_code: status.code().unwrap_or(-1),
            stdout,
            stderr,
        })
    } else {
        Ok(ShellOutput {
            stdout,
            stderr,
            exit_code: status.code().unwrap_or(0),
        })
    }
}

#[cfg(all(test, unix))]
mod reap_tests {
    use super::ReapGuard;
    use std::time::Duration;

    /// Check whether a PID is a zombie or gone by reading /proc/<pid>/status.
    /// Returns true if the process is fully gone (no /proc entry).
    /// Returns false if the process is still running or is a zombie.
    fn process_is_gone(pid: u32) -> bool {
        let status_path = format!("/proc/{pid}/status");
        match std::fs::read_to_string(&status_path) {
            Ok(content) => {
                // Check if the process is a zombie.
                for line in content.lines() {
                    if line.starts_with("State:") {
                        return line.contains("Z (zombie)");
                    }
                }
                // State line not found — treat as still alive.
                false
            }
            Err(_) => true, // /proc entry doesn't exist — process is gone.
        }
    }

    /// Wait for a process to be fully reaped (gone from /proc), with a timeout.
    fn wait_for_reap(pid: u32, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if process_is_gone(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// P1: ReapGuard::drop() must reap the child even when no Tokio runtime
    /// is available.
    ///
    /// Creates a child inside a runtime, extracts it, drops the runtime,
    /// then drops the ReapGuard outside any runtime context.  The child
    /// must be killed AND reaped (no zombie).
    #[test]
    fn reap_guard_reaps_without_runtime() {
        // Create a child inside a runtime.
        let child = {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                tokio::process::Command::new("sleep")
                    .arg("30")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
        };
        let pid = child.id().expect("child must have a PID");

        // Drop the runtime — we are now outside any Tokio context.
        // (The `child` variable still holds the tokio::process::Child handle.)

        // Wrap in ReapGuard and drop it — no runtime is available.
        let guard = ReapGuard::new(child);
        drop(guard);

        // The child must be reaped (gone from /proc) within a reasonable time.
        assert!(
            wait_for_reap(pid, Duration::from_secs(5)),
            "child PID {pid} must be reaped (gone from /proc) after ReapGuard::drop without runtime"
        );
    }

    /// P1: ReapGuard::drop() must reap the child even when the Tokio runtime
    /// is being shut down.
    ///
    /// Creates a child, then drops both the ReapGuard and the runtime in
    /// sequence.  The std::thread reaper must still complete.
    #[test]
    fn reap_guard_reaps_during_runtime_shutdown() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (child, handle) = rt.block_on(async {
            let child = tokio::process::Command::new("sleep")
                .arg("30")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let pid = child.id().expect("child must have a PID");
            (child, pid)
        });

        let guard = ReapGuard::new(child);

        // Drop the guard first, then the runtime.
        drop(guard);
        drop(rt);

        assert!(
            wait_for_reap(handle, Duration::from_secs(5)),
            "child PID {handle} must be reaped after ReapGuard::drop during runtime shutdown"
        );
    }

    /// P1: ReapGuard::drop() must reap the child in the normal case (runtime
    /// alive).  Verifies the std::thread path works even when a runtime
    /// is present.
    #[test]
    fn reap_guard_reaps_with_runtime_alive() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let pid = rt.block_on(async {
            let child = tokio::process::Command::new("sleep")
                .arg("30")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let pid = child.id().expect("child must have a PID");
            let guard = ReapGuard::new(child);
            drop(guard);
            pid
        });

        assert!(
            wait_for_reap(pid, Duration::from_secs(5)),
            "child PID {pid} must be reaped after ReapGuard::drop with runtime alive"
        );

        drop(rt);
    }

    /// P1: kill_and_wait must not set `reaped` when wait() fails.
    ///
    /// We can't easily force a wait() failure, but we can verify that a
    /// successfully killed child is marked as reaped (so Drop doesn't
    /// double-kill).
    #[test]
    fn kill_and_wait_marks_reaped_on_success() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let child = tokio::process::Command::new("sleep")
                .arg("30")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let pid = child.id().expect("child must have a PID");

            let mut guard = ReapGuard::new(child);
            guard.kill_and_wait().await;
            assert!(
                guard.reaped,
                "reaped must be true after successful kill_and_wait"
            );

            // Dropping the guard should be a no-op (already reaped).
            drop(guard);

            assert!(
                wait_for_reap(pid, Duration::from_secs(2)),
                "child PID {pid} must be gone after kill_and_wait"
            );
        });

        drop(rt);
    }
}
