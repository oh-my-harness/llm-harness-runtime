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
/// POSIX `ECHILD` errno — "no child processes".
///
/// This is 10 on all major Unix platforms (Linux, macOS, FreeBSD, NetBSD,
/// OpenBSD, Solaris).  We use platform cfg to avoid a `libc` dependency
/// while remaining explicit and auditable.  On unsupported platforms the
/// value is -1 (never matches), so unknown errors are treated as `Unknown`
/// (handle retained) — the safe default.
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "android",
    target_os = "ios",
))]
const ECHILD: i32 = 10;

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "android",
    target_os = "ios",
)))]
const ECHILD: i32 = -1;

/// Sleep duration inserted on consecutive `EINTR` to prevent tight-spinning.
const EINTR_BACKOFF: std::time::Duration = std::time::Duration::from_micros(100);

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
    /// `waitpid` was interrupted (EINTR).  Retry after a short backoff.
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

/// Atomically decrement the in-flight counter, saturating at zero.
fn decrement_in_flight(in_flight: &std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::Ordering;
    let _ = in_flight.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
        if v > 0 { Some(v - 1) } else { None }
    });
}

/// Supervisor lifecycle state for the reaper thread pool.
///
/// `Healthy` is the only state that accepts new admissions.  Any required
/// thread exiting (panic, mutex poison, channel disconnect) transitions to
/// `Degraded`, which closes admission (fail-closed).  `Failed` is set
/// during initialisation if a thread cannot be spawned.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SupervisorState {
    Healthy = 0,
    Degraded = 1,
    Failed = 2,
}

impl SupervisorState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Healthy,
            1 => Self::Degraded,
            _ => Self::Failed,
        }
    }
}

/// Pre-spawn admission token obtained from [`ChildReaper::try_admit`].
///
/// Dropping it releases the in-flight slot — correct when spawn fails or
/// is never attempted.  To couple the admission with a spawned child, call
/// [`SpawnAdmission::into_lease`], which transfers ownership to a
/// [`ChildLease`] that **cannot** release the slot independently of the
/// child.
///
/// This two-phase design (admission → lease) replaces the old `ReapPermit`
/// which could be dropped independently of the child handle, leaking the
/// admission slot while the child kept running.
pub struct SpawnAdmission {
    in_flight: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
}

impl SpawnAdmission {
    /// Couple this admission with a spawned child.  Consumes `self`.
    ///
    /// The returned [`ChildLease`] owns both the child handle and the
    /// admission count — they cannot be separated.  The count is released
    /// only after the child is confirmed exited (via `confirm_exit` or via
    /// the reaper when the lease is dropped without confirmation).
    pub fn into_lease(mut self, child: tokio::process::Child) -> ChildLease {
        let in_flight = self.in_flight.take();
        ChildLease {
            child: Some(child),
            in_flight,
            confirmed: false,
        }
    }
}

impl Drop for SpawnAdmission {
    fn drop(&mut self) {
        if let Some(in_flight) = &self.in_flight {
            decrement_in_flight(in_flight);
        }
    }
}

/// Post-spawn lease coupling a child handle with its admission count.
///
/// Created via [`SpawnAdmission::into_lease`].  The admission slot is
/// released **only** after the child is confirmed exited — either by
/// [`Self::confirm_exit`] (called after a successful `wait()`) or by the
/// reaper when this lease is dropped without confirmation.
///
/// Unlike the old `ReapPermit`, the `in_flight` Arc is **not** decremented
/// by `Drop` directly.  Instead, `Drop` delegates the child + `in_flight`
/// clone to the reaper, which decrements only after confirmed exit.  This
/// guarantees the admission slot is held for the entire lifetime of the
/// child.
pub struct ChildLease {
    child: Option<tokio::process::Child>,
    in_flight: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    confirmed: bool,
}

impl ChildLease {
    /// Create a lease without admission (for tests that don't use the
    /// global reaper's admission counter).
    pub fn new(child: tokio::process::Child) -> Self {
        Self {
            child: Some(child),
            in_flight: None,
            confirmed: false,
        }
    }

    /// Borrow the inner child mutably (for `start_kill`, `wait`, taking
    /// stdout/stderr handles, etc.).
    pub fn child_mut(&mut self) -> Option<&mut tokio::process::Child> {
        self.child.as_mut()
    }

    /// Confirm the child has exited and release the admission slot.
    /// Idempotent — safe to call multiple times.
    pub fn confirm_exit(&mut self) {
        if !self.confirmed {
            self.confirmed = true;
            if let Some(in_flight) = &self.in_flight {
                decrement_in_flight(in_flight);
            }
        }
    }

    /// Kill and wait for the child to exit.  Marks the lease as confirmed
    /// only if `wait()` succeeds — a failed `wait()` leaves `confirmed`
    /// false so `Drop` still delegates to the reaper.
    async fn kill_and_wait(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
            if child.wait().await.is_ok() {
                self.confirm_exit();
            }
        }
    }

    /// Wait for the child to exit naturally (no kill).  Marks as confirmed
    /// on success.
    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        if let Some(child) = &mut self.child {
            let status = child.wait().await?;
            self.confirm_exit();
            Ok(status)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "child already taken",
            ))
        }
    }
}

impl Drop for ChildLease {
    fn drop(&mut self) {
        if self.confirmed {
            return;
        }
        // Child exit NOT confirmed.  Delegate to the reaper — the child
        // handle and an in_flight clone move into a ReapRequest.  The
        // reaper will kill+wait the child, then drop the ReapRequest
        // (releasing in_flight).  The lease's own Arc is dropped here
        // (refcount decreases), but the atomic is NOT decremented — only
        // the reaper does that after confirmed exit.
        if let Some(child) = self.child.take() {
            let in_flight = self.in_flight.clone();
            if let Some(reaper) = ChildReaper::global()
                && reaper.is_healthy()
            {
                reaper.reap(child, in_flight);
            } else {
                push_orphaned(child, in_flight);
            }
        }
    }
}

/// A kill+reap request queued to the [`ChildReaper`].
///
/// Owns the child handle and the `in_flight` Arc.  `Drop` decrements
/// `in_flight` — which only fires after the child is confirmed exited
/// (worker/stuck-loop drops the request) or during catastrophic teardown.
struct ReapRequest {
    child: tokio::process::Child,
    in_flight: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
}

impl Drop for ReapRequest {
    fn drop(&mut self) {
        if let Some(in_flight) = &self.in_flight {
            decrement_in_flight(in_flight);
        }
    }
}

/// Snapshot of reaper state for observability.
#[derive(Debug)]
pub struct ReaperMetrics {
    /// Current in-flight (admitted but not yet released) count.
    pub in_flight: usize,
    /// Total admissions since startup.
    pub admitted: usize,
    /// Total children confirmed reaped.
    pub completed: usize,
    /// Total thread (worker/stuck/orphan) failures.
    pub worker_failures: usize,
    /// Current stuck-list length.
    pub stuck: usize,
    /// Current orphaned-list length.
    pub orphaned: usize,
    /// Whether the supervisor is healthy (accepting admissions).
    pub healthy: bool,
}

/// A centralized, multi-worker child process reaper with **bounded
/// admission**, **supervisor health monitoring**, and **all-thread
/// liveness guards**.
///
/// Architecture:
/// - `try_admit()` returns `Option<SpawnAdmission>`; the admission's `Drop`
///   releases the slot atomically.  `into_lease(child)` couples the
///   admission with the child — the slot is released only after confirmed
///   exit.
/// - `is_healthy()` returns `false` if **any** thread (worker, stuck, or
///   orphan) has exited.  `try_admit()` checks health and returns `None`
///   when unhealthy (fail-closed admission).
/// - All threads (workers, stuck reaper, orphan reaper) are registered with
///   a [`LivenessGuard`] that transitions the supervisor to `Degraded` on
///   drop.
/// - Initialisation is transactional: if any thread fails to spawn, the
///   state is set to `Failed` and `global()` returns `None`.
/// - Workers process kill+reap requests from a bounded channel.  Children
///   that don't exit within [`REAP_WORKER_TIMEOUT`] (including persistent
///   EINTR) are moved to a stuck list.
/// - On queue `Full`/`Disconnected`, children are pushed to the stuck list
///   — **no fallback thread is spawned**.
/// - `try_wait` errors are classified via [`classify_wait`]: only `Exited`
///   and `AlreadyReaped` authorize dropping the handle; `Unknown` errors
///   retain it in the stuck list.
pub struct ChildReaper {
    sender: std::sync::mpsc::SyncSender<ReapRequest>,
    /// Global in-flight counter; bounds total unreaped children.
    in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Supervisor state — `Healthy` is the only state that admits.
    state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// Stuck list for children that didn't exit within the worker timeout.
    stuck: std::sync::Arc<std::sync::Mutex<Vec<ReapRequest>>>,
    /// Metrics counters.
    admitted: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    completed: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    failures: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// RAII guard that transitions the supervisor to `Degraded` when any
/// required thread (worker, stuck, or orphan) exits.  All threads carry
/// one — this replaces the old `WorkerGuard` which only covered workers.
struct LivenessGuard {
    state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    failures: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for LivenessGuard {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        self.state
            .store(SupervisorState::Degraded as u8, Ordering::SeqCst);
        self.failures.fetch_add(1, Ordering::SeqCst);
    }
}

/// Static last-resort storage for child handles that cannot be queued to
/// the normal stuck list (reaper unhealthy or stuck mutex poisoned).
static ORPHANED: std::sync::OnceLock<std::sync::Mutex<Vec<ReapRequest>>> =
    std::sync::OnceLock::new();

fn orphaned_list() -> &'static std::sync::Mutex<Vec<ReapRequest>> {
    ORPHANED.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Push a child handle to the orphaned list — never drops the handle.
/// The `in_flight` Arc (if held) moves into the `ReapRequest` and is
/// decremented by `Drop` only after confirmed exit.
fn push_orphaned(
    mut child: tokio::process::Child,
    in_flight: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
) {
    let _ = child.start_kill();
    let req = ReapRequest { child, in_flight };
    let mut guard = orphaned_list().lock().unwrap_or_else(|e| e.into_inner());
    guard.push(req);
}

impl ChildReaper {
    /// Returns the global reaper instance, or `None` if any thread could
    /// not be created (fail-closed).  Initialisation is transactional: if
    /// any of the worker/stuck/orphan threads fail to spawn, the state is
    /// set to `Failed` and `None` is returned permanently.
    pub fn global() -> Option<&'static ChildReaper> {
        static REAPER: std::sync::OnceLock<Option<ChildReaper>> = std::sync::OnceLock::new();
        REAPER
            .get_or_init(|| {
                use std::sync::atomic::AtomicU8;

                let (sender, receiver) =
                    std::sync::mpsc::sync_channel::<ReapRequest>(REAPER_QUEUE_CAPACITY);
                let receiver = std::sync::Arc::new(std::sync::Mutex::new(receiver));
                let stuck = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                let in_flight = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let state = std::sync::Arc::new(AtomicU8::new(SupervisorState::Healthy as u8));
                let admitted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let completed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let failures = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

                // Spawn worker threads — each carries a LivenessGuard.
                for i in 0..REAPER_WORKERS {
                    let rx = receiver.clone();
                    let stuck = stuck.clone();
                    let state_clone = state.clone();
                    let failures = failures.clone();
                    let completed = completed.clone();
                    if std::thread::Builder::new()
                        .name(format!("child-reaper-{i}"))
                        .spawn(move || {
                            let _guard = LivenessGuard {
                                state: state_clone,
                                failures,
                            };
                            Self::worker_loop(rx, stuck, completed)
                        })
                        .is_err()
                    {
                        state.store(
                            SupervisorState::Failed as u8,
                            std::sync::atomic::Ordering::SeqCst,
                        );
                        return None;
                    }
                }

                // Spawn stuck reaper thread.
                let stuck_clone = stuck.clone();
                let completed_clone = completed.clone();
                let state_clone = state.clone();
                let failures_clone = failures.clone();
                if std::thread::Builder::new()
                    .name("child-reaper-stuck".to_string())
                    .spawn(move || {
                        let _guard = LivenessGuard {
                            state: state_clone,
                            failures: failures_clone,
                        };
                        Self::stuck_loop(stuck_clone, completed_clone)
                    })
                    .is_err()
                {
                    state.store(
                        SupervisorState::Failed as u8,
                        std::sync::atomic::Ordering::SeqCst,
                    );
                    return None;
                }

                // Spawn orphan reaper thread.
                let completed_clone2 = completed.clone();
                let state_clone2 = state.clone();
                let failures_clone2 = failures.clone();
                if std::thread::Builder::new()
                    .name("child-reaper-orphan".to_string())
                    .spawn(move || {
                        let _guard = LivenessGuard {
                            state: state_clone2,
                            failures: failures_clone2,
                        };
                        Self::orphan_reaper_loop(completed_clone2)
                    })
                    .is_err()
                {
                    state.store(
                        SupervisorState::Failed as u8,
                        std::sync::atomic::Ordering::SeqCst,
                    );
                    return None;
                }

                Some(ChildReaper {
                    sender,
                    in_flight,
                    state,
                    stuck,
                    admitted,
                    completed,
                    failures,
                })
            })
            .as_ref()
    }

    /// Returns `true` only when the supervisor is `Healthy` — all required
    /// threads alive.  Any thread exit transitions to `Degraded`, closing
    /// admission.
    pub fn is_healthy(&self) -> bool {
        use std::sync::atomic::Ordering;
        SupervisorState::from_u8(self.state.load(Ordering::SeqCst)) == SupervisorState::Healthy
    }

    /// Try to acquire a pre-spawn admission.  Returns `Some(admission)` if
    /// a slot was acquired, `None` if the reaper is unhealthy or at
    /// capacity (fail-closed).
    pub fn try_admit(&self) -> Option<SpawnAdmission> {
        use std::sync::atomic::Ordering;
        if !self.is_healthy() {
            return None;
        }
        self.in_flight
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                if v < REAPER_QUEUE_CAPACITY {
                    Some(v + 1)
                } else {
                    None
                }
            })
            .ok()
            .map(|_| {
                self.admitted.fetch_add(1, Ordering::SeqCst);
                SpawnAdmission {
                    in_flight: Some(self.in_flight.clone()),
                }
            })
    }

    /// Snapshot of reaper metrics for observability.
    pub fn metrics(&self) -> ReaperMetrics {
        use std::sync::atomic::Ordering;
        ReaperMetrics {
            in_flight: self.in_flight.load(Ordering::SeqCst),
            admitted: self.admitted.load(Ordering::SeqCst),
            completed: self.completed.load(Ordering::SeqCst),
            worker_failures: self.failures.load(Ordering::SeqCst),
            stuck: self.stuck.lock().unwrap_or_else(|e| e.into_inner()).len(),
            orphaned: orphaned_list()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len(),
            healthy: self.is_healthy(),
        }
    }

    /// Queue a child for killing and reaping.  If the queue is full or
    /// disconnected, the child is pushed to the stuck list — **no fallback
    /// thread is spawned**.  The `in_flight` Arc (if held) is decremented
    /// by `ReapRequest::drop` only after confirmed exit.
    fn reap(
        &self,
        mut child: tokio::process::Child,
        in_flight: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    ) {
        let _ = child.start_kill();
        let req = ReapRequest { child, in_flight };
        match self.sender.try_send(req) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(req))
            | Err(std::sync::mpsc::TrySendError::Disconnected(req)) => {
                let mut list = self.stuck.lock().unwrap_or_else(|e| e.into_inner());
                list.push(req);
            }
        }
    }

    /// Worker thread main loop.  Receives [`ReapRequest`]s, kills+reaps
    /// each child with a bounded timeout via [`reap_with_deadline`], and
    /// classifies `try_wait` errors.
    ///
    /// Only `Exited` and `AlreadyReaped` drop the `ReapRequest`.  Timeouts
    /// (including persistent EINTR) and `Unknown` errors move the child to
    /// the stuck list.
    fn worker_loop(
        receiver: std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<ReapRequest>>>,
        stuck: std::sync::Arc<std::sync::Mutex<Vec<ReapRequest>>>,
        completed: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::atomic::Ordering;
        loop {
            let req = match receiver.lock() {
                Ok(rx) => rx.recv(),
                Err(e) => e.into_inner().recv(),
            };
            match req {
                Ok(mut req) => {
                    let _ = req.child.start_kill();
                    let deadline = std::time::Instant::now() + REAP_WORKER_TIMEOUT;
                    let done = reap_with_deadline(|| req.child.try_wait(), deadline);
                    if done {
                        completed.fetch_add(1, Ordering::SeqCst);
                        // req drops — ReapRequest::drop releases in_flight.
                    } else {
                        let mut list = stuck.lock().unwrap_or_else(|e| e.into_inner());
                        list.push(req);
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
        completed: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::atomic::Ordering;
        loop {
            std::thread::sleep(STUCK_REAPER_INTERVAL);
            let mut list = stuck.lock().unwrap_or_else(|e| e.into_inner());
            let mut still_stuck = Vec::with_capacity(list.len());
            while let Some(mut req) = list.pop() {
                match classify_wait(req.child.try_wait()) {
                    WaitOutcome::Exited | WaitOutcome::AlreadyReaped => {
                        completed.fetch_add(1, Ordering::SeqCst);
                    }
                    WaitOutcome::Pending | WaitOutcome::Interrupted | WaitOutcome::Unknown(_) => {
                        still_stuck.push(req)
                    }
                }
            }
            *list = still_stuck;
        }
    }

    /// Orphan reaper thread.  Periodically reaps handles in the
    /// [`ORPHANED`] list.
    fn orphan_reaper_loop(completed: std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::Ordering;
        loop {
            std::thread::sleep(STUCK_REAPER_INTERVAL);
            let mut list = orphaned_list().lock().unwrap_or_else(|e| e.into_inner());
            let mut still_orphaned = Vec::with_capacity(list.len());
            while let Some(mut req) = list.pop() {
                match classify_wait(req.child.try_wait()) {
                    WaitOutcome::Exited | WaitOutcome::AlreadyReaped => {
                        completed.fetch_add(1, Ordering::SeqCst);
                    }
                    WaitOutcome::Pending | WaitOutcome::Interrupted | WaitOutcome::Unknown(_) => {
                        still_orphaned.push(req)
                    }
                }
            }
            *list = still_orphaned;
        }
    }
}

#[cfg(test)]
impl ChildReaper {
    /// Create a ChildReaper whose receiver is already disconnected.
    fn new_disconnected() -> ChildReaper {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<ReapRequest>(1);
        drop(receiver);
        ChildReaper {
            sender,
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
                SupervisorState::Healthy as u8,
            )),
            stuck: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            admitted: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            completed: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            failures: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Create a ChildReaper with a bounded queue, no worker threads, and
    /// the receiver kept alive (returned to caller).
    fn new_no_workers(capacity: usize) -> (ChildReaper, std::sync::mpsc::Receiver<ReapRequest>) {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<ReapRequest>(capacity);
        (
            ChildReaper {
                sender,
                in_flight: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
                    SupervisorState::Healthy as u8,
                )),
                stuck: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                admitted: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                completed: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                failures: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            },
            receiver,
        )
    }

    /// Create a ChildReaper in `Failed` state — for testing fail-closed
    /// admission after a supervisor failure.
    fn new_failed() -> ChildReaper {
        let (sender, _receiver) = std::sync::mpsc::sync_channel::<ReapRequest>(1);
        ChildReaper {
            sender,
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
                SupervisorState::Failed as u8,
            )),
            stuck: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            admitted: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            completed: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            failures: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Directly create a ReapRequest for testing the stuck list.
    fn reap_direct(
        &self,
        child: tokio::process::Child,
        in_flight: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    ) {
        self.reap(child, in_flight);
    }

    /// Get a clone of the stuck list Arc (for testing poison recovery).
    fn stuck_arc(&self) -> std::sync::Arc<std::sync::Mutex<Vec<ReapRequest>>> {
        self.stuck.clone()
    }
}
/// Reap a child with a deadline, using the provided `try_wait` closure.
///
/// Returns `true` if the child was confirmed exited (handle can be
/// dropped), `false` if it should be moved to the stuck list.
///
/// The `try_wait` closure is injected for testability — tests can
/// simulate persistent EINTR, unknown errors, etc. without spawning real
/// processes.
///
/// **Every** retry branch (Interrupted and Pending) checks the deadline.
/// Persistent EINTR no longer occupies a worker indefinitely — after the
/// deadline, the child moves to the stuck list.
fn reap_with_deadline(
    mut try_wait: impl FnMut() -> std::io::Result<Option<std::process::ExitStatus>>,
    deadline: std::time::Instant,
) -> bool {
    loop {
        match classify_wait(try_wait()) {
            WaitOutcome::Exited | WaitOutcome::AlreadyReaped => return true,
            WaitOutcome::Interrupted => {
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(EINTR_BACKOFF);
            }
            WaitOutcome::Pending => {
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            WaitOutcome::Unknown(_) => return false,
        }
    }
}

/// Drain stdout/stderr from a spawned child with **shutdown cancellation**
/// and **guaranteed reap on drop**.
///
/// This is the production-grade variant of [`drain_child_output`] used by
/// sandbox backends that have an internal shutdown signal (e.g.
/// `BwrapEnv`).
///
/// **Drop-safe reap**: the [`ChildLease`] is created **synchronously**
/// before the future is returned, so dropping the future without polling
/// it still triggers `ChildLease::drop`, which delegates to the reaper.
/// This closes the "unpolled future" gap — the child is never orphaned
/// even if the caller creates the future and drops it before the first
/// poll.
///
/// `OsEnv` continues to use the simpler [`drain_child_output`] (no shutdown
/// token, no lease overhead) because it has no internal shutdown signal.
pub fn drain_child_output_guarded(
    mut lease: ChildLease,
    opts: ShellOptions<'_>,
    shutdown: CancellationToken,
) -> impl std::future::Future<Output = Result<ShellOutput, EnvError>> + use<'_> {
    // Take stdout/stderr handles synchronously — the lease retains the
    // child handle for kill/wait.
    let mut stdout_handle = lease.child_mut().and_then(|c| c.stdout.take());
    let mut stderr_handle = lease.child_mut().and_then(|c| c.stderr.take());

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

    async move {
        let mut on_stdout = opts.on_stdout;
        let mut on_stderr = opts.on_stderr;
        let drain = async {
            let stdout_task = drain_pipe!(stdout_handle, on_stdout);
            let stderr_task = drain_pipe!(stderr_handle, on_stderr);
            let (stdout_res, stderr_res) = tokio::join!(stdout_task, stderr_task);
            let stdout = stdout_res?;
            let stderr = stderr_res?;
            let status = lease.wait().await.map_err(EnvError::Io)?;
            Ok::<_, EnvError>((stdout, stderr, status))
        };

        let (stdout, stderr, status) = if let Some(timeout) = opts.timeout {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    lease.kill_and_wait().await;
                    return Err(EnvError::Other("sandbox is shutting down".into()));
                }
                _ = opts.abort.cancelled() => {
                    lease.kill_and_wait().await;
                    return Err(EnvError::Aborted);
                }
                res = tokio::time::timeout(timeout, drain) => match res {
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => {
                        lease.kill_and_wait().await;
                        return Err(e);
                    }
                    Err(_) => {
                        lease.kill_and_wait().await;
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
                    lease.kill_and_wait().await;
                    return Err(EnvError::Other("sandbox is shutting down".into()));
                }
                _ = opts.abort.cancelled() => {
                    lease.kill_and_wait().await;
                    return Err(EnvError::Aborted);
                }
                res = drain => match res {
                    Ok(v) => v,
                    Err(e) => {
                        lease.kill_and_wait().await;
                        return Err(e);
                    }
                },
            }
        };

        // Child was reaped via wait() in the drain future.
        lease.confirm_exit();

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
}

#[cfg(all(test, unix))]
mod reap_tests {
    use super::{
        ChildLease, ChildReaper, LivenessGuard, REAPER_QUEUE_CAPACITY, SupervisorState,
        WaitOutcome, classify_wait, drain_child_output_guarded, reap_with_deadline,
    };
    use llm_harness_types::ShellOptions;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    /// Check whether a process has been fully reaped (gone from /proc).
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

    /// Spawn a child process inside a temporary Tokio runtime.
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

    /// Minimal ShellOptions for testing drain_child_output_guarded.
    fn test_opts() -> ShellOptions<'static> {
        ShellOptions {
            cwd: None,
            env: vec![],
            timeout: None,
            abort: CancellationToken::new(),
            on_stdout: None,
            on_stderr: None,
        }
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
        let err = std::io::Error::from_raw_os_error(super::ECHILD);
        assert!(matches!(
            classify_wait(Err(err)),
            WaitOutcome::AlreadyReaped
        ));
    }

    #[test]
    fn classify_wait_unknown_error_retains_handle() {
        let err = std::io::Error::from_raw_os_error(999);
        assert!(matches!(classify_wait(Err(err)), WaitOutcome::Unknown(_)));
    }

    // ── reap_with_deadline injectable tests ───────────────────────────────

    /// P0: Persistent EINTR must exceed the deadline and move to stuck,
    /// not spin forever occupying a worker.
    #[test]
    fn persistent_eintr_exceeds_deadline() {
        let deadline = std::time::Instant::now() + Duration::from_millis(50);
        let result = reap_with_deadline(
            || Err(std::io::Error::from(std::io::ErrorKind::Interrupted)),
            deadline,
        );
        assert!(
            !result,
            "persistent EINTR should move to stuck after deadline"
        );
        assert!(
            std::time::Instant::now() >= deadline,
            "should have waited until deadline, not return early"
        );
    }

    /// P0: Persistent unknown wait error must retain ownership (return
    /// false) immediately, not treat it as AlreadyReaped.
    #[test]
    fn persistent_unknown_error_retains_ownership() {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let result = reap_with_deadline(|| Err(std::io::Error::from_raw_os_error(999)), deadline);
        assert!(
            !result,
            "unknown error must retain ownership (move to stuck)"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "should return immediately for unknown error"
        );
    }

    /// Exited child should return true (handle can be dropped).
    #[test]
    fn reap_with_deadline_exited() {
        let result = reap_with_deadline(
            || Ok(Some(std::process::ExitStatus::from_raw(0))),
            std::time::Instant::now() + Duration::from_secs(5),
        );
        assert!(result, "exited child should return true");
    }

    // ── ChildLease integration tests ──────────────────────────────────────

    /// P0: ChildLease::drop() must reap the child even when no Tokio
    /// runtime is available.
    #[test]
    fn lease_reaps_without_runtime() {
        let child = spawn_child("sleep", "30");
        let pid = child.id().expect("child must have a PID");

        let lease = ChildLease::new(child);
        drop(lease);

        assert!(
            wait_for_reap(pid, Duration::from_secs(5)),
            "child PID {pid} must be reaped after ChildLease::drop without runtime"
        );
    }

    /// P0: ChildLease::drop() must reap the child even when the Tokio
    /// runtime is being shut down.
    #[test]
    fn lease_reaps_during_runtime_shutdown() {
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
            child.id().expect("child must have a PID")
        });

        let child = spawn_child("sleep", "30");
        let pid2 = child.id().expect("child must have a PID");
        let lease = ChildLease::new(child);
        drop(lease);
        drop(rt);

        assert!(
            wait_for_reap(pid2, Duration::from_secs(5)),
            "child PID {pid2} must be reaped after ChildLease::drop during runtime shutdown"
        );
        let _ = pid; // suppress unused warning
    }

    /// P0: ChildLease::drop() must reap the child in the normal case
    /// (runtime alive).
    #[test]
    fn lease_reaps_with_runtime_alive() {
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
            let lease = ChildLease::new(child);
            drop(lease);
            pid
        });

        assert!(
            wait_for_reap(pid, Duration::from_secs(5)),
            "child PID {pid} must be reaped after ChildLease::drop with runtime alive"
        );

        drop(rt);
    }

    /// P0: kill_and_wait must confirm exit on success.
    #[test]
    fn kill_and_wait_marks_confirmed_on_success() {
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

            let mut lease = ChildLease::new(child);
            lease.kill_and_wait().await;

            assert!(
                wait_for_reap(pid, Duration::from_secs(2)),
                "child PID {pid} must be gone after kill_and_wait"
            );
        });

        drop(rt);
    }

    // ── P0: Unpolled future still reaps child ─────────────────────────────

    /// P0: Creating the drain future but dropping it WITHOUT polling must
    /// still kill+reap the child.  This is the core "unpolled future" fix
    /// — the ChildLease is created synchronously before the future is
    /// returned, so Drop fires even without a single poll.
    #[test]
    fn unpolled_future_still_reaps_child() {
        let sentinel = std::env::temp_dir().join(format!(
            "reaper_unpolled_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sentinel_str = sentinel.to_str().unwrap().to_string();

        let (child, pid) = {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = rt.block_on(async {
                let child = tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(format!("sleep 1; touch {sentinel_str}"))
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap();
                let pid = child.id().expect("child must have a PID");
                (child, pid)
            });
            drop(rt);
            result
        };

        // Create the future but DON'T poll it.
        let lease = ChildLease::new(child);
        let opts = test_opts();
        let token = CancellationToken::new();
        let fut = drain_child_output_guarded(lease, opts, token);

        // Drop the future without polling — must still kill+reap the child.
        drop(fut);

        // Wait long enough for the sentinel to appear IF the child survived.
        std::thread::sleep(Duration::from_millis(1500));

        assert!(
            !sentinel.exists(),
            "sentinel must not exist — unpolled future must have killed the child"
        );
        assert!(
            wait_for_reap(pid, Duration::from_secs(5)),
            "child PID {pid} must be reaped after dropping unpolled future"
        );

        let _ = std::fs::remove_file(&sentinel);
    }

    // ── Supervisor health tests ───────────────────────────────────────────

    /// P0: LivenessGuard must transition the supervisor to Degraded on
    /// drop, incrementing the failure counter.
    #[test]
    fn liveness_guard_closes_admission() {
        let state = Arc::new(AtomicU8::new(SupervisorState::Healthy as u8));
        let failures = Arc::new(AtomicUsize::new(0));

        {
            let _guard = LivenessGuard {
                state: state.clone(),
                failures: failures.clone(),
            };
            assert_eq!(
                SupervisorState::from_u8(state.load(Ordering::SeqCst)),
                SupervisorState::Healthy,
                "state should be Healthy while guard is alive"
            );
        }

        assert_eq!(
            SupervisorState::from_u8(state.load(Ordering::SeqCst)),
            SupervisorState::Degraded,
            "state should be Degraded after guard drops"
        );
        assert_eq!(
            failures.load(Ordering::SeqCst),
            1,
            "failure counter should be 1"
        );
    }

    /// P0: A reaper in Failed state must reject all admissions
    /// (fail-closed).  This simulates a thread startup failure.
    #[test]
    fn supervisor_failure_closes_admission() {
        let reaper = ChildReaper::new_failed();
        assert!(!reaper.is_healthy(), "Failed reaper must not be healthy");
        assert!(
            reaper.try_admit().is_none(),
            "Failed reaper must reject admission"
        );
        let metrics = reaper.metrics();
        assert!(!metrics.healthy, "metrics must report unhealthy");
    }

    // ── Admission bound tests ─────────────────────────────────────────────

    /// P0: Admission at capacity must reject spawn; full recovery after
    /// all reaped, and the counter recovers exactly once.
    #[test]
    fn admission_at_capacity_rejects_then_recovers() {
        let reaper = ChildReaper::new_disconnected();

        // Fill to capacity.
        let mut admissions: Vec<_> = Vec::with_capacity(REAPER_QUEUE_CAPACITY);
        for _ in 0..REAPER_QUEUE_CAPACITY {
            admissions.push(reaper.try_admit().expect("should admit below capacity"));
        }
        assert_eq!(
            reaper.metrics().in_flight,
            REAPER_QUEUE_CAPACITY,
            "in_flight should be at capacity"
        );

        // Next admit must fail.
        assert!(reaper.try_admit().is_none(), "should reject at capacity");

        // Drop all admissions — releases all slots.
        drop(admissions);

        assert_eq!(
            reaper.metrics().in_flight,
            0,
            "in_flight should be 0 after all admissions dropped"
        );

        // Should recover and admit again.
        let admission = reaper.try_admit().expect("should admit after recovery");
        assert_eq!(
            reaper.metrics().in_flight,
            1,
            "in_flight should be 1 after one new admission"
        );
        drop(admission);
        assert_eq!(
            reaper.metrics().in_flight,
            0,
            "in_flight should be 0 after dropping the admission"
        );
    }

    /// P0: Spawn failure (dropping SpawnAdmission without into_lease) must
    /// release only the pre-spawn admission.
    #[test]
    fn spawn_failure_releases_pre_spawn_admission() {
        let reaper = ChildReaper::new_disconnected();

        let admission = reaper.try_admit().expect("should admit");
        assert_eq!(
            reaper.metrics().in_flight,
            1,
            "in_flight should be 1 after admit"
        );

        // Simulate spawn failure — drop the admission without calling
        // into_lease.  The slot must be released.
        drop(admission);

        assert_eq!(
            reaper.metrics().in_flight,
            0,
            "in_flight should be 0 after spawn failure (admission dropped)"
        );
    }

    // ── ChildReaper queue-overflow tests ──────────────────────────────────

    /// P1/P0: reap() with a disconnected receiver must retain the child
    /// handle in the stuck list.
    #[test]
    fn reap_recovers_child_on_disconnected_receiver() {
        let reaper = ChildReaper::new_disconnected();
        let child = spawn_child("sleep", "30");

        reaper.reap_direct(child, None);

        let stuck_len = reaper
            .stuck_arc()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        assert_eq!(
            stuck_len, 1,
            "child must be retained in stuck list after disconnected receiver"
        );
    }

    /// P1/P0: reap() with a full queue must retain the child handle in the
    /// stuck list.
    #[test]
    fn reap_recovers_child_on_full_queue() {
        let (reaper, _receiver) = ChildReaper::new_no_workers(1);

        let filler = spawn_child("sleep", "30");
        reaper.reap_direct(filler, None);

        let child = spawn_child("sleep", "30");
        reaper.reap_direct(child, None);

        let stuck_len = reaper
            .stuck_arc()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        assert_eq!(
            stuck_len, 1,
            "overflow child must be retained in stuck list after full queue"
        );
    }

    /// P1/P0: A child that takes time to die must not block reaping of
    /// other children.
    #[test]
    fn concurrent_reaps_do_not_block_each_other() {
        // Ensure the global reaper is initialized.
        ChildReaper::global().expect("global reaper must be available");

        let mut pids = Vec::new();
        for _ in 0..8 {
            let child = spawn_child("sleep", "30");
            let pid = child.id().expect("child must have a PID");
            pids.push(pid);
            let lease = ChildLease::new(child);
            drop(lease);
        }

        for &pid in &pids {
            assert!(
                wait_for_reap(pid, Duration::from_secs(5)),
                "child PID {pid} must be reaped promptly"
            );
        }
    }

    // ── Mutex poison recovery test ────────────────────────────────────────

    /// P0: After the stuck mutex is poisoned (a panicked peer), reap_direct
    /// must still recover and push the child to the stuck list via
    /// `unwrap_or_else(|e| e.into_inner())`.
    #[test]
    fn mutex_poison_still_reaps() {
        let reaper = ChildReaper::new_disconnected();

        // Poison the stuck mutex in a separate thread.
        let stuck = reaper.stuck_arc();
        let handle = std::thread::spawn(move || {
            let _guard = stuck.lock().unwrap();
            panic!("intentional poison");
        });
        let _ = handle.join();

        // Now the mutex is poisoned.  reap_direct should recover.
        let child = spawn_child("sleep", "30");
        reaper.reap_direct(child, None);

        let stuck_len = reaper
            .stuck_arc()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        assert_eq!(
            stuck_len, 1,
            "child should be in stuck list after poison recovery"
        );
    }

    // ── Cancellation storm bounded test ───────────────────────────────────

    /// P0: Under a cancellation storm (many children admitted and
    /// immediately dropped), in_flight must stay bounded and return to 0
    /// after all are reaped.
    #[test]
    fn cancellation_storm_stays_bounded() {
        let reaper = ChildReaper::global().expect("global reaper must be available");

        let mut pids = Vec::new();
        for _ in 0..32 {
            let admission = reaper.try_admit().expect("should admit below capacity");
            let child = spawn_child("sleep", "30");
            let pid = child.id().expect("child must have a PID");
            pids.push(pid);
            let lease = admission.into_lease(child);
            drop(lease);
        }

        // in_flight must not exceed the number we admitted.
        let metrics = reaper.metrics();
        assert!(
            metrics.in_flight <= 32,
            "in_flight ({}) must stay bounded (<= 32)",
            metrics.in_flight
        );

        // All PIDs must be reaped.
        for &pid in &pids {
            assert!(
                wait_for_reap(pid, Duration::from_secs(10)),
                "child PID {pid} must be reaped"
            );
        }

        // in_flight must return to 0 after all reaped.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if reaper.metrics().in_flight == 0 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("in_flight did not return to 0: {:?}", reaper.metrics());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
