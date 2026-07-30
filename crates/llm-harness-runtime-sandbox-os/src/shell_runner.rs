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

/// A centralized, single-threaded child process reaper.
///
/// Instead of spawning one OS thread per dropped child (which risks
/// unbounded thread creation and panics in `Drop` when thread creation
/// fails), this struct owns a **single** dedicated reaper thread that
/// processes kill+reap requests from an MPSC channel.
///
/// The reaper thread is created lazily on first use.  If thread creation
/// fails (extreme resource exhaustion), `global()` returns `None` and
/// callers must fall back to SIGKILL-only (init reaps the zombie when the
/// parent process exits).
struct ChildReaper {
    sender: std::sync::mpsc::Sender<tokio::process::Child>,
}

impl ChildReaper {
    /// Returns the global reaper instance, or `None` if the reaper thread
    /// could not be created.
    fn global() -> Option<&'static ChildReaper> {
        static REAPER: std::sync::OnceLock<Option<ChildReaper>> = std::sync::OnceLock::new();
        REAPER
            .get_or_init(|| {
                let (sender, receiver) = std::sync::mpsc::channel::<tokio::process::Child>();
                match std::thread::Builder::new()
                    .name("child-reaper".to_string())
                    .spawn(move || Self::reaper_loop(receiver))
                {
                    Ok(_) => Some(ChildReaper { sender }),
                    Err(_) => None,
                }
            })
            .as_ref()
    }

    /// The reaper thread's main loop.  Receives children from the channel
    /// and kills+reaps each one sequentially.
    fn reaper_loop(receiver: std::sync::mpsc::Receiver<tokio::process::Child>) {
        while let Ok(mut child) = receiver.recv() {
            let _ = child.start_kill();
            Self::reap_one(&mut child);
        }
    }

    /// Kill and reap a single child, with bounded retry on `try_wait()`
    /// errors and a timeout for stuck (D-state) processes.
    fn reap_one(child: &mut tokio::process::Child) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut consecutive_errors = 0u32;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break, // Reaped successfully.
                Ok(None) => {
                    consecutive_errors = 0;
                    if std::time::Instant::now() >= deadline {
                        // Child didn't exit within 10s after SIGKILL
                        // (e.g., D-state / uninterruptible sleep).
                        // Give up — init will reap when parent exits.
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= 10 {
                        // Persistent error — likely ECHILD (already reaped
                        // by something else).  Stop retrying.
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
    }

    /// Queue a child for killing and reaping.  Does not panic and does not
    /// create a new thread.
    fn reap(&self, mut child: tokio::process::Child) {
        let _ = child.start_kill();
        let _ = self.sender.send(child);
    }
}

/// Guard that owns a [`tokio::process::Child`] and guarantees the child is
/// killed **and reaped** even if the drain future is dropped without being
/// polled to completion.
///
/// `tokio::process::Command::kill_on_drop(true)` only sends `SIGKILL` from
/// `Drop` — it cannot `await`, so the child is not reaped (reaped via Tokio's
/// best-effort SIGCHLD orphan queue, which requires the signal driver and is
/// not guaranteed).  This guard closes that gap by delegating to a
/// **centralized [`ChildReaper`]** — a single dedicated OS thread that
/// polls `try_wait()` (synchronous `waitpid` with `WNOHANG`) until the child
/// exits, ensuring the zombie is reaped regardless of Tokio runtime state.
///
/// Using a centralized reaper (instead of spawning one thread per child)
/// avoids unbounded thread creation and prevents panics in `Drop` when
/// thread creation fails.  The reaper thread is independent of the Tokio
/// runtime lifecycle: it survives runtime shutdown and works when no
/// runtime exists at all.
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
        if let Some(child) = self.child.take() {
            // Delegate to the centralized reaper.  This does not create a
            // new thread and cannot panic — the child is queued for the
            // single dedicated reaper thread, which kills and reaps it via
            // `try_wait()` polling.
            //
            // If the reaper thread is unavailable (creation failed on first
            // use due to extreme resource exhaustion), the child is
            // SIGKILL'd here and will be reaped by init (PID 1) when the
            // parent process exits.  This is degraded but safe.
            if let Some(reaper) = ChildReaper::global() {
                reaper.reap(child);
            } else {
                let mut child = child;
                let _ = child.start_kill();
            }
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

    /// Check whether a process has been fully reaped (gone from /proc).
    ///
    /// Returns `true` only when `/proc/<pid>/status` cannot be read — the
    /// process is fully gone from the process table.  A zombie still has a
    /// `/proc` entry (it occupies a PID until reaped), so it returns
    /// `false`.
    fn process_is_gone(pid: u32) -> bool {
        let status_path = format!("/proc/{pid}/status");
        std::fs::read_to_string(&status_path).is_err()
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
