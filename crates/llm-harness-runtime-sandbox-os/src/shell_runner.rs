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

/// Bounded capacity of the reaper queue.
const REAPER_QUEUE_CAPACITY: usize = 256;

/// Number of worker threads processing kill+reap requests.
const REAPER_WORKERS: usize = 4;

/// Per-child timeout in a worker before moving the child to the stuck list.
/// Prevents one stuck (D-state) child from blocking a worker indefinitely.
const REAP_WORKER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How often the stuck reaper thread checks stuck children.
const STUCK_REAPER_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// A centralized, multi-worker child process reaper.
///
/// Instead of spawning one OS thread per dropped child (which risks
/// unbounded thread creation and panics in `Drop` when thread creation
/// fails), this struct owns a pool of dedicated reaper threads that
/// process kill+reap requests from a **bounded** MPSC channel.  Multiple
/// workers ensure one stuck child cannot block reaping of others.
///
/// Children that don't exit within [`REAP_WORKER_TIMEOUT`] are moved to a
/// stuck list and revisited periodically by a dedicated thread — their
/// handles are never dropped until `try_wait` confirms exit, so no zombie
/// survives.
///
/// The reaper threads are created lazily on first use.  If thread creation
/// fails (extreme resource exhaustion), `global()` returns `None` and
/// callers fall back to reaping in a dedicated fallback thread (or inline
/// if even that fails).  No path drops the child handle without reaping.
struct ChildReaper {
    sender: std::sync::mpsc::SyncSender<tokio::process::Child>,
    /// Kept alive so worker threads and the stuck reaper can access the
    /// shared list; not read directly after construction.
    #[allow(dead_code)]
    stuck: std::sync::Arc<std::sync::Mutex<Vec<tokio::process::Child>>>,
}

impl ChildReaper {
    /// Returns the global reaper instance, or `None` if the reaper threads
    /// could not be created.
    fn global() -> Option<&'static ChildReaper> {
        static REAPER: std::sync::OnceLock<Option<ChildReaper>> = std::sync::OnceLock::new();
        REAPER
            .get_or_init(|| {
                let (sender, receiver) =
                    std::sync::mpsc::sync_channel::<tokio::process::Child>(REAPER_QUEUE_CAPACITY);
                let receiver = std::sync::Arc::new(std::sync::Mutex::new(receiver));
                let stuck = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

                for i in 0..REAPER_WORKERS {
                    let rx = receiver.clone();
                    let stuck = stuck.clone();
                    if std::thread::Builder::new()
                        .name(format!("child-reaper-{i}"))
                        .spawn(move || Self::worker_loop(rx, stuck))
                        .is_err()
                    {
                        return None;
                    }
                }

                let stuck_clone = stuck.clone();
                if std::thread::Builder::new()
                    .name("child-reaper-stuck".to_string())
                    .spawn(move || Self::stuck_loop(stuck_clone))
                    .is_err()
                {
                    return None;
                }

                Some(ChildReaper { sender, stuck })
            })
            .as_ref()
    }

    /// Worker thread main loop.  Receives children from the shared
    /// receiver and kills+reaps each one with a bounded timeout.  Children
    /// that don't exit within the timeout are moved to the stuck list so
    /// their handles are not dropped.
    fn worker_loop(
        receiver: std::sync::Arc<
            std::sync::Mutex<std::sync::mpsc::Receiver<tokio::process::Child>>,
        >,
        stuck: std::sync::Arc<std::sync::Mutex<Vec<tokio::process::Child>>>,
    ) {
        loop {
            let child = match receiver.lock() {
                Ok(rx) => rx.recv(),
                Err(_) => break, // Mutex poisoned — exit.
            };
            match child {
                Ok(mut child) => {
                    let _ = child.start_kill();
                    let deadline = std::time::Instant::now() + REAP_WORKER_TIMEOUT;
                    let mut reaped = false;
                    loop {
                        match child.try_wait() {
                            Ok(Some(_)) => {
                                reaped = true;
                                break;
                            }
                            Ok(None) => {
                                if std::time::Instant::now() >= deadline {
                                    break; // Move to stuck list.
                                }
                                std::thread::sleep(std::time::Duration::from_millis(10));
                            }
                            Err(_) => {
                                reaped = true; // Likely ECHILD (already reaped).
                                break;
                            }
                        }
                    }
                    if !reaped {
                        match stuck.lock() {
                            Ok(mut list) => list.push(child),
                            Err(_) => Self::reap_to_completion(&mut child),
                        }
                    }
                }
                Err(_) => break, // Channel disconnected — exit.
            }
        }
    }

    /// Stuck reaper thread.  Periodically revisits children that didn't
    /// exit within the worker timeout.  Children are never dropped until
    /// `try_wait` confirms exit.
    fn stuck_loop(stuck: std::sync::Arc<std::sync::Mutex<Vec<tokio::process::Child>>>) {
        loop {
            std::thread::sleep(STUCK_REAPER_INTERVAL);
            let mut list = match stuck.lock() {
                Ok(list) => list,
                Err(_) => break,
            };
            let mut still_stuck = Vec::with_capacity(list.len());
            while let Some(mut child) = list.pop() {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => {}
                    Ok(None) => still_stuck.push(child),
                }
            }
            *list = still_stuck;
        }
    }

    /// Queue a child for killing and reaping.  If the queue is full or
    /// disconnected, reaps the child in a fallback thread (or inline if
    /// thread creation fails).  No path drops the child handle without
    /// reaping.
    fn reap(&self, mut child: tokio::process::Child) {
        let _ = child.start_kill();
        match self.sender.try_send(child) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(child))
            | Err(std::sync::mpsc::TrySendError::Disconnected(child)) => {
                reap_with_fallback(child);
            }
        }
    }

    /// Kill and reap a child, never giving up.  Loops with `try_wait`
    /// until the child exits or `try_wait` returns an error (ECHILD).
    /// For truly stuck (D-state) processes, this loops indefinitely —
    /// the handle is never dropped, so no zombie is left.
    fn reap_to_completion(child: &mut tokio::process::Child) {
        loop {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
    }
}

#[cfg(test)]
impl ChildReaper {
    /// Create a ChildReaper whose receiver is already disconnected.
    /// Used to test the `SendError` recovery path.
    fn new_disconnected() -> ChildReaper {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        drop(receiver);
        ChildReaper {
            sender,
            stuck: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Create a ChildReaper with a bounded queue and no worker threads.
    /// The queue can be filled to test the `Full` fallback path.
    fn new_no_workers(capacity: usize) -> ChildReaper {
        let (sender, _receiver) = std::sync::mpsc::sync_channel(capacity);
        ChildReaper {
            sender,
            stuck: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
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
/// **centralized [`ChildReaper`]** — a pool of dedicated OS threads that
/// poll `try_wait()` (synchronous `waitpid` with `WNOHANG`) until the child
/// exits, ensuring the zombie is reaped regardless of Tokio runtime state.
///
/// Using a centralized reaper pool (instead of spawning one thread per
/// child) avoids unbounded thread creation and prevents panics in `Drop`
/// when thread creation fails.  The reaper threads are independent of the Tokio
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
            // Delegate to the centralized reaper.  If the reaper is
            // unavailable (thread creation failed on first use), spawn a
            // fallback thread to reap — or reap inline as a last resort.
            // No path drops the child handle without reaping.
            if let Some(reaper) = ChildReaper::global() {
                reaper.reap(child);
            } else {
                reap_with_fallback(child);
            }
        }
    }
}

/// Reap a child in a fallback thread, or inline if thread creation fails.
///
/// Uses a shared `Arc<Mutex<Option>>` so the child handle is conditionally
/// moved into the thread only when `spawn` succeeds.  If `spawn` fails, the
/// handle is recovered and reaped inline (blocking the caller).  No path
/// drops the handle without reaping.
fn reap_with_fallback(mut child: tokio::process::Child) {
    let _ = child.start_kill();
    let shared = std::sync::Arc::new(std::sync::Mutex::new(Some(child)));
    let thread_shared = shared.clone();
    if std::thread::Builder::new()
        .name("child-reap-fallback".to_string())
        .spawn(move || {
            if let Some(mut c) = thread_shared.lock().unwrap().take() {
                ChildReaper::reap_to_completion(&mut c);
            }
        })
        .is_err()
        && let Some(mut c) = shared.lock().unwrap().take()
    {
        ChildReaper::reap_to_completion(&mut c);
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
    use super::{ChildReaper, ReapGuard, reap_with_fallback};
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

    /// Spawn a child process inside a temporary Tokio runtime (required by
    /// `tokio::process::Command::spawn`).  The runtime is dropped after
    /// spawning — the child handle remains valid for `start_kill` and
    /// `try_wait`, which are synchronous and don't need a runtime.
    fn spawn_child(cmd: &str, arg: &str) -> tokio::process::Child {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            tokio::process::Command::new(cmd)
                .arg(arg)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
    }

    /// P1/P0: reap() with a disconnected receiver must still reap the child.
    ///
    /// The old code ignored `SendError<Child>`, dropping the handle without
    /// `wait()`.  The PID remained in /proc as a zombie.  The fix recovers
    /// the child from `SendError` and reaps it via a fallback thread.
    #[test]
    fn reap_recovers_child_on_disconnected_receiver() {
        let reaper = ChildReaper::new_disconnected();
        let child = spawn_child("sleep", "30");
        let pid = child.id().expect("child must have a PID");

        reaper.reap(child);

        assert!(
            wait_for_reap(pid, Duration::from_secs(5)),
            "child PID {pid} must be reaped after reap() with disconnected receiver"
        );
    }

    /// P1/P0: reap() with a full queue must still reap the child.
    ///
    /// When the bounded queue is saturated, `try_send` returns `Full`.  The
    /// child must be recovered and reaped via the fallback path, not dropped.
    #[test]
    fn reap_recovers_child_on_full_queue() {
        let reaper = ChildReaper::new_no_workers(1);

        // Fill the queue with a killed child.
        let filler = spawn_child("sleep", "30");
        let filler_pid = filler.id().expect("filler must have a PID");
        reaper.reap(filler);

        // Now reap a second child — try_send will fail with Full.
        let child = spawn_child("sleep", "30");
        let pid = child.id().expect("child must have a PID");
        reaper.reap(child);

        assert!(
            wait_for_reap(pid, Duration::from_secs(5)),
            "child PID {pid} must be reaped after reap() with full queue"
        );

        // The filler was killed by reap() and will be reaped by the OS
        // since no workers are draining the queue.
        assert!(
            wait_for_reap(filler_pid, Duration::from_secs(5)),
            "filler PID {filler_pid} must be gone"
        );
    }

    /// P1/P0: A stuck child must not block reaping of other children.
    ///
    /// With multiple worker threads, one slow-to-exit child does not
    /// prevent other children from being reaped promptly.
    #[test]
    fn stuck_child_does_not_block_healthy_children() {
        let reaper = ChildReaper::global().expect("global reaper must be available");

        let slow_child = spawn_child("sleep", "30");
        let slow_pid = slow_child.id().expect("slow child must have a PID");
        reaper.reap(slow_child);

        // Submit multiple healthy children that exit quickly.
        let mut pids = Vec::new();
        for _ in 0..4 {
            let child = spawn_child("true", "");
            let pid = child.id().expect("child must have a PID");
            pids.push(pid);
            reaper.reap(child);
        }

        for &pid in &pids {
            assert!(
                wait_for_reap(pid, Duration::from_secs(3)),
                "healthy child PID {pid} must be reaped promptly despite a slow sibling"
            );
        }

        assert!(
            wait_for_reap(slow_pid, Duration::from_secs(10)),
            "slow child PID {slow_pid} must eventually be reaped"
        );
    }

    /// P1/P0: reap_to_completion never gives up — it loops until the child
    /// exits.  No path drops the handle without confirming exit status.
    #[test]
    fn reap_to_completion_reaps_killed_child() {
        let child = spawn_child("sleep", "30");
        let pid = child.id().expect("child must have a PID");

        let mut child = child;
        let _ = child.start_kill();
        ChildReaper::reap_to_completion(&mut child);

        assert!(
            process_is_gone(pid),
            "child PID {pid} must be gone after reap_to_completion"
        );
    }

    /// P1/P0: reap_with_fallback reaps the child via a fallback thread.
    #[test]
    fn reap_with_fallback_reaps_child() {
        let child = spawn_child("sleep", "30");
        let pid = child.id().expect("child must have a PID");

        reap_with_fallback(child);

        assert!(
            wait_for_reap(pid, Duration::from_secs(5)),
            "child PID {pid} must be reaped after reap_with_fallback"
        );
    }
}
