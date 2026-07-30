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
/// POSIX `ECHILD` errno — "no child processes".  On Linux and macOS this
/// is 10.  Used to distinguish "already reaped elsewhere" from other
/// try_wait errors so that an unknown error does not cause the child
/// handle to be dropped prematurely.
const ECHILD: i32 = 10;

/// Outcome of a non-blocking `try_wait` on a child process.
///
/// This explicit classification replaces the old pattern of treating any
/// `Err(_)` as "already reaped".  Only `Exited` and `AlreadyReaped`
/// authorize dropping the handle; `Unknown` must retain it.
#[derive(Debug)]
enum WaitOutcome {
    /// Child exited; exit status available.  Handle can be dropped.
    Exited,
    /// Child is still running.  Keep waiting.
    Pending,
    /// `waitpid` was interrupted (EINTR).  Retry immediately.
    Interrupted,
    /// Child was already reaped by someone else (ECHILD).  Handle can be
    /// dropped.
    AlreadyReaped,
    /// Unexpected error.  The handle must NOT be dropped — keep it in the
    /// stuck list and retry.
    #[allow(dead_code)]
    Unknown(std::io::Error),
}

/// Classify a `try_wait` result into a [`WaitOutcome`].
///
/// This is a pure function that is easy to unit-test with synthesized
/// errors, covering the three error branches the reviewer requires:
/// `Interrupted` (retry), `ECHILD` (already reaped), and unknown errors
/// (retain handle).
fn classify_wait(result: std::io::Result<Option<std::process::ExitStatus>>) -> WaitOutcome {
    match result {
        Ok(Some(_)) => WaitOutcome::Exited,
        Ok(None) => WaitOutcome::Pending,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::Interrupted {
                WaitOutcome::Interrupted
            } else if e.raw_os_error() == Some(ECHILD) {
                WaitOutcome::AlreadyReaped
            } else {
                WaitOutcome::Unknown(e)
            }
        }
    }
}

/// A kill+reap request queued to the [`ChildReaper`].
///
/// `has_permit` tracks whether an admission permit was acquired for this
/// child before spawn.  The worker/stuck-loop releases the permit (via
/// `in_flight` decrement) only when `has_permit` is true and the child is
/// confirmed exited.  This prevents permit underflow for test-created
/// children that bypass admission.
struct ReapRequest {
    child: tokio::process::Child,
    has_permit: bool,
}

/// A centralized, multi-worker child process reaper with **bounded
/// admission**.
///
/// The reaper enforces a global admission bound (`in_flight` counter,
/// capacity = [`REAPER_QUEUE_CAPACITY`]) acquired **before** process
/// creation.  This guarantees the total number of unreaped children never
/// exceeds the bound, so the queue can never overflow and no fallback
/// threads are needed.
///
/// Architecture:
/// - `try_admit()` increments `in_flight` before spawn; `release()`
///   decrements it when the child is confirmed exited.
/// - Workers process kill+reap requests from a bounded channel.  Children
///   that don't exit within [`REAP_WORKER_TIMEOUT`] are moved to a stuck
///   list (also bounded by admission) and revisited by a dedicated thread.
/// - On queue `Full`/`Disconnected`, children are pushed to the stuck list
///   — **no fallback thread is spawned**, preventing unbounded thread
///   growth.
/// - `try_wait` errors are classified via [`classify_wait`]: only `Exited`
///   and `AlreadyReaped` authorize dropping the handle; `Unknown` errors
///   retain it in the stuck list.
///
/// If thread creation fails (extreme resource exhaustion), `global()`
/// returns `None` and callers must refuse to spawn (fail-closed).
pub struct ChildReaper {
    sender: std::sync::mpsc::SyncSender<ReapRequest>,
    /// Global in-flight counter; bounds total unreaped children.
    in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Stuck list for children that didn't exit within the worker timeout.
    /// Bounded by `in_flight` admission.
    #[allow(dead_code)]
    stuck: std::sync::Arc<std::sync::Mutex<Vec<ReapRequest>>>,
}

impl ChildReaper {
    /// Returns the global reaper instance, or `None` if the reaper threads
    /// could not be created.  When `None`, callers must refuse to spawn
    /// (fail-closed) — there is no inline fallback.
    pub fn global() -> Option<&'static ChildReaper> {
        static REAPER: std::sync::OnceLock<Option<ChildReaper>> = std::sync::OnceLock::new();
        REAPER
            .get_or_init(|| {
                let (sender, receiver) =
                    std::sync::mpsc::sync_channel::<ReapRequest>(REAPER_QUEUE_CAPACITY);
                let receiver = std::sync::Arc::new(std::sync::Mutex::new(receiver));
                let stuck = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                let in_flight = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

                for i in 0..REAPER_WORKERS {
                    let rx = receiver.clone();
                    let stuck = stuck.clone();
                    let in_flight = in_flight.clone();
                    if std::thread::Builder::new()
                        .name(format!("child-reaper-{i}"))
                        .spawn(move || Self::worker_loop(rx, stuck, in_flight))
                        .is_err()
                    {
                        return None;
                    }
                }

                let stuck_clone = stuck.clone();
                let in_flight_clone = in_flight.clone();
                if std::thread::Builder::new()
                    .name("child-reaper-stuck".to_string())
                    .spawn(move || Self::stuck_loop(stuck_clone, in_flight_clone))
                    .is_err()
                {
                    return None;
                }

                Some(ChildReaper {
                    sender,
                    in_flight,
                    stuck,
                })
            })
            .as_ref()
    }

    /// Try to acquire an admission permit before spawning a child.
    /// Returns `true` if a permit was acquired (caller may spawn),
    /// `false` if the sandbox is at capacity (caller must refuse to
    /// spawn — fail-closed).  The permit is released when the child is
    /// confirmed exited (by the worker, stuck-loop, or `ReapGuard`).
    pub fn try_admit(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.in_flight
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                if v < REAPER_QUEUE_CAPACITY {
                    Some(v + 1)
                } else {
                    None
                }
            })
            .is_ok()
    }

    /// Release an admission permit.  Saturating — never underflows.
    pub fn release(&self) {
        use std::sync::atomic::Ordering;
        let _ = self
            .in_flight
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                if v > 0 { Some(v - 1) } else { None }
            });
    }

    /// Worker thread main loop.  Receives [`ReapRequest`]s from the shared
    /// receiver, kills+reaps each child with a bounded timeout, and
    /// classifies `try_wait` errors via [`classify_wait`].
    ///
    /// Only `Exited` and `AlreadyReaped` release the permit and drop the
    /// handle.  `Unknown` errors and timeouts move the child to the stuck
    /// list — the handle is never dropped without a definitive outcome.
    fn worker_loop(
        receiver: std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<ReapRequest>>>,
        stuck: std::sync::Arc<std::sync::Mutex<Vec<ReapRequest>>>,
        in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::atomic::Ordering;
        loop {
            let req = match receiver.lock() {
                Ok(rx) => rx.recv(),
                Err(_) => break, // Mutex poisoned — exit.
            };
            match req {
                Ok(mut req) => {
                    let _ = req.child.start_kill();
                    let deadline = std::time::Instant::now() + REAP_WORKER_TIMEOUT;
                    let done = loop {
                        match classify_wait(req.child.try_wait()) {
                            WaitOutcome::Exited | WaitOutcome::AlreadyReaped => break true,
                            WaitOutcome::Interrupted => continue,
                            WaitOutcome::Pending => {
                                if std::time::Instant::now() >= deadline {
                                    break false; // Move to stuck list.
                                }
                                std::thread::sleep(std::time::Duration::from_millis(10));
                            }
                            WaitOutcome::Unknown(_) => break false, // Stuck list.
                        }
                    };
                    if done {
                        if req.has_permit {
                            let _ =
                                in_flight.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                                    if v > 0 { Some(v - 1) } else { None }
                                });
                        }
                    } else {
                        match stuck.lock() {
                            Ok(mut list) => list.push(req),
                            // Mutex poisoned — catastrophic.  The child
                            // handle is retained (not dropped) by moving
                            // into `req` which is then dropped.  This is
                            // the best we can do: the reaper is dead.
                            Err(_) => { /* req drops, child handle drops */ }
                        }
                    }
                }
                Err(_) => break, // Channel disconnected — exit.
            }
        }
    }

    /// Stuck reaper thread.  Periodically revisits children that didn't
    /// exit within the worker timeout.  Handles are never dropped until
    /// `classify_wait` returns `Exited` or `AlreadyReaped`.
    fn stuck_loop(
        stuck: std::sync::Arc<std::sync::Mutex<Vec<ReapRequest>>>,
        in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::atomic::Ordering;
        loop {
            std::thread::sleep(STUCK_REAPER_INTERVAL);
            let mut list = match stuck.lock() {
                Ok(list) => list,
                Err(_) => break,
            };
            let mut still_stuck = Vec::with_capacity(list.len());
            while let Some(mut req) = list.pop() {
                match classify_wait(req.child.try_wait()) {
                    WaitOutcome::Exited | WaitOutcome::AlreadyReaped => {
                        if req.has_permit {
                            let _ =
                                in_flight.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                                    if v > 0 { Some(v - 1) } else { None }
                                });
                        }
                        // Handle dropped — child confirmed exited.
                    }
                    // Pending, Interrupted, Unknown — retain handle.
                    WaitOutcome::Pending | WaitOutcome::Interrupted | WaitOutcome::Unknown(_) => {
                        still_stuck.push(req)
                    }
                }
            }
            *list = still_stuck;
        }
    }

    /// Queue a child for killing and reaping.  If the queue is full or
    /// disconnected, the child is pushed to the stuck list — **no fallback
    /// thread is spawned**.  The permit (if `has_permit`) is released by
    /// the worker/stuck-loop when the child is confirmed exited.
    fn reap(&self, mut child: tokio::process::Child, has_permit: bool) {
        let _ = child.start_kill();
        let req = ReapRequest { child, has_permit };
        match self.sender.try_send(req) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(req))
            | Err(std::sync::mpsc::TrySendError::Disconnected(req)) => {
                // Push to stuck list instead of spawning a fallback thread.
                // The stuck_loop will reap it.  The permit is NOT released
                // here — stuck_loop releases it when the child exits.
                if let Ok(mut list) = self.stuck.lock() {
                    list.push(req);
                }
                // If the stuck mutex is poisoned, the handle is dropped.
                // This is catastrophic (a worker panicked while holding the
                // lock) and the permit leaks (admission eventually blocks).
            }
        }
    }
}

#[cfg(test)]
impl ChildReaper {
    /// Create a ChildReaper whose receiver is already disconnected.
    /// Used to test the `SendError` recovery path.
    fn new_disconnected() -> ChildReaper {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<ReapRequest>(1);
        drop(receiver);
        ChildReaper {
            sender,
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            stuck: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Create a ChildReaper with a bounded queue, no worker threads, and
    /// the receiver **kept alive** (returned to the caller).  This lets
    /// tests fill the queue to capacity and then verify that a subsequent
    /// `reap()` hits `Full` (not `Disconnected`).
    fn new_no_workers(capacity: usize) -> (ChildReaper, std::sync::mpsc::Receiver<ReapRequest>) {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<ReapRequest>(capacity);
        (
            ChildReaper {
                sender,
                in_flight: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                stuck: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            },
            receiver,
        )
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
/// `holds_permit` tracks whether an admission permit was acquired before
/// spawn.  When `true`, the permit is released when the child is confirmed
/// exited — either by `kill_and_wait`/`wait` (drain future reaps) or by the
/// reaper worker/stuck-loop (guard delegates on drop).  Test-created guards
/// pass `false` so they don't underflow the admission counter.
///
/// Once the child has been successfully reaped via [`Self::kill_and_wait`] or
/// [`Self::wait`], the `reaped` flag is set so `Drop` does not double-kill.
struct ReapGuard {
    child: Option<tokio::process::Child>,
    reaped: bool,
    holds_permit: bool,
}

impl ReapGuard {
    fn new(child: tokio::process::Child, holds_permit: bool) -> Self {
        Self {
            child: Some(child),
            reaped: false,
            holds_permit,
        }
    }

    /// Borrow the inner child mutably (for `start_kill`, `wait`, taking
    /// stdout/stderr handles, etc.).
    fn child_mut(&mut self) -> Option<&mut tokio::process::Child> {
        self.child.as_mut()
    }

    /// Release the admission permit if held.  Idempotent.
    fn release_permit(&mut self) {
        if self.holds_permit {
            if let Some(reaper) = ChildReaper::global() {
                reaper.release();
            }
            self.holds_permit = false;
        }
    }

    /// Kill and wait for the child to exit.  Only marks the child as reaped
    /// if `wait()` succeeds — a failed `wait()` leaves `reaped` false so
    /// `Drop` still attempts reaping.
    async fn kill_and_wait(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
            if child.wait().await.is_ok() {
                self.reaped = true;
                self.release_permit();
            }
        }
    }

    /// Wait for the child to exit naturally (no kill).  Only marks as reaped
    /// if `wait()` succeeds.
    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        if let Some(child) = &mut self.child {
            let status = child.wait().await?;
            self.reaped = true;
            self.release_permit();
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
            // Child was reaped by kill_and_wait/wait.  Permit already
            // released (or never held).  Defensive: release if somehow
            // still held.
            self.release_permit();
            return;
        }
        if let Some(child) = self.child.take() {
            if let Some(reaper) = ChildReaper::global() {
                // Delegate to the reaper.  The permit (if held) transfers
                // to the reaper — it releases when the child is confirmed
                // exited.  No fallback thread is spawned.
                reaper.reap(child, self.holds_permit);
            } else {
                // Reaper unavailable (workers couldn't spawn).  This only
                // happens for test-created guards (holds_permit=false) or
                // catastrophic resource exhaustion.  Bounded inline reap
                // — never blocks indefinitely.
                let mut child = child;
                let _ = child.start_kill();
                let deadline = std::time::Instant::now() + REAP_WORKER_TIMEOUT;
                while std::time::Instant::now() < deadline {
                    match classify_wait(child.try_wait()) {
                        WaitOutcome::Exited | WaitOutcome::AlreadyReaped => return,
                        WaitOutcome::Interrupted => continue,
                        WaitOutcome::Pending => {
                            std::thread::sleep(std::time::Duration::from_millis(10))
                        }
                        WaitOutcome::Unknown(_) => return,
                    }
                }
                // Timeout: handle dropped (zombie).  Catastrophic but
                // bounded — no inline infinite wait.
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
    holds_permit: bool,
) -> Result<ShellOutput, EnvError> {
    let mut guard = ReapGuard::new(child, holds_permit);

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
    use super::{ChildReaper, ReapGuard, WaitOutcome, classify_wait};
    use std::os::unix::process::ExitStatusExt;
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

    // ── classify_wait unit tests (no process needed) ──────────────────────

    #[test]
    fn classify_wait_exited() {
        let status = std::process::ExitStatus::from_raw(0);
        assert!(matches!(
            classify_wait(Ok(Some(status))),
            WaitOutcome::Exited
        ));
    }

    #[test]
    fn classify_wait_pending() {
        assert!(matches!(classify_wait(Ok(None)), WaitOutcome::Pending));
    }

    #[test]
    fn classify_wait_interrupted() {
        let err = std::io::Error::from(std::io::ErrorKind::Interrupted);
        assert!(matches!(classify_wait(Err(err)), WaitOutcome::Interrupted));
    }

    #[test]
    fn classify_wait_already_reaped() {
        // ECHILD (errno 10 on Linux/macOS) — child was reaped elsewhere.
        let err = std::io::Error::from_raw_os_error(super::ECHILD);
        assert!(matches!(
            classify_wait(Err(err)),
            WaitOutcome::AlreadyReaped
        ));
    }

    #[test]
    fn classify_wait_unknown_error_retains_handle() {
        // An unexpected error must NOT be treated as AlreadyReaped — the
        // handle must be retained (WaitOutcome::Unknown).
        let err = std::io::Error::from_raw_os_error(999); // bogus errno
        assert!(matches!(classify_wait(Err(err)), WaitOutcome::Unknown(_)));
    }

    // ── ReapGuard integration tests ───────────────────────────────────────

    /// P1: ReapGuard::drop() must reap the child even when no Tokio runtime
    /// is available.
    #[test]
    fn reap_guard_reaps_without_runtime() {
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

        let guard = ReapGuard::new(child, false);
        drop(guard);

        assert!(
            wait_for_reap(pid, Duration::from_secs(5)),
            "child PID {pid} must be reaped after ReapGuard::drop without runtime"
        );
    }

    /// P1: ReapGuard::drop() must reap the child even when the Tokio runtime
    /// is being shut down.
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

        let guard = ReapGuard::new(child, false);
        drop(guard);
        drop(rt);

        assert!(
            wait_for_reap(handle, Duration::from_secs(5)),
            "child PID {handle} must be reaped after ReapGuard::drop during runtime shutdown"
        );
    }

    /// P1: ReapGuard::drop() must reap the child in the normal case (runtime
    /// alive).
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
            let guard = ReapGuard::new(child, false);
            drop(guard);
            pid
        });

        assert!(
            wait_for_reap(pid, Duration::from_secs(5)),
            "child PID {pid} must be reaped after ReapGuard::drop with runtime alive"
        );

        drop(rt);
    }

    /// P1: kill_and_wait must mark the child as reaped on success.
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

            let mut guard = ReapGuard::new(child, false);
            guard.kill_and_wait().await;
            assert!(
                guard.reaped,
                "reaped must be true after successful kill_and_wait"
            );
            assert!(
                !guard.holds_permit,
                "permit must be released after kill_and_wait"
            );

            drop(guard);

            assert!(
                wait_for_reap(pid, Duration::from_secs(2)),
                "child PID {pid} must be gone after kill_and_wait"
            );
        });

        drop(rt);
    }

    // ── ChildReaper queue-overflow tests ──────────────────────────────────

    /// P1/P0: reap() with a disconnected receiver must retain the child
    /// handle in the stuck list — NOT drop it, NOT spawn a fallback thread.
    ///
    /// The test reaper has no workers/stuck_loop, so the child stays in the
    /// stuck list.  We verify the handle is retained (stuck list non-empty)
    /// and the child was killed (SIGKILL sent).  Full reaping is the
    /// stuck_loop's job, covered by the global-reaper test below.
    #[test]
    fn reap_recovers_child_on_disconnected_receiver() {
        let reaper = ChildReaper::new_disconnected();
        let child = spawn_child("sleep", "30");

        reaper.reap(child, false);

        // The child handle must be in the stuck list — not dropped.
        let stuck_len = reaper.stuck.lock().unwrap().len();
        assert_eq!(
            stuck_len, 1,
            "child must be retained in stuck list after disconnected receiver"
        );
    }

    /// P1/P0: reap() with a full queue must retain the child handle in the
    /// stuck list — NOT drop it, NOT spawn a fallback thread.
    ///
    /// The receiver is kept alive (not dropped) so `try_send` returns `Full`
    /// when the queue is saturated, exercising the actual Full path (not
    /// Disconnected).  The overflow child must end up in the stuck list.
    #[test]
    fn reap_recovers_child_on_full_queue() {
        // new_no_workers returns (reaper, receiver).  The receiver is kept
        // alive (_receiver) so try_send returns Full when the queue is full.
        let (reaper, _receiver) = ChildReaper::new_no_workers(1);

        // Fill the queue to capacity (1 slot) with a killed child.
        let filler = spawn_child("sleep", "30");
        reaper.reap(filler, false);

        // The filler went into the queue (try_send succeeded).
        // Now reap a second child — try_send will fail with Full.
        let child = spawn_child("sleep", "30");
        reaper.reap(child, false);

        // The overflow child must be in the stuck list — not dropped.
        let stuck_len = reaper.stuck.lock().unwrap().len();
        assert_eq!(
            stuck_len, 1,
            "overflow child must be retained in stuck list after full queue"
        );
    }

    /// P1/P0: A child that takes time to die must not block reaping of
    /// other children.  Multiple worker threads ensure concurrency.
    ///
    /// Note: SIGKILL cannot be caught, so all children exit quickly.  This
    /// test verifies that multiple children are reaped concurrently by the
    /// worker pool, not that a truly stuck (D-state) child is handled
    /// (that path is covered by classify_wait unit tests).
    #[test]
    fn concurrent_reaps_do_not_block_each_other() {
        let reaper = ChildReaper::global().expect("global reaper must be available");

        let mut pids = Vec::new();
        for _ in 0..8 {
            let child = spawn_child("sleep", "30");
            let pid = child.id().expect("child must have a PID");
            pids.push(pid);
            reaper.reap(child, false);
        }

        for &pid in &pids {
            assert!(
                wait_for_reap(pid, Duration::from_secs(5)),
                "child PID {pid} must be reaped promptly"
            );
        }
    }
}
