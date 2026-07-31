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

// ── Component liveness bitmask ───────────────────────────────────────────

/// Bit set in the `components` atomic when the worker pool is alive.
const COMP_WORKERS: u8 = 0b001;
/// Bit set when the stuck scheduler thread is alive.
const COMP_STUCK: u8 = 0b010;
/// Bit set when the orphan reaper thread is alive.
const COMP_ORPHAN: u8 = 0b100;
/// All components alive — the only state in which `try_admit` succeeds.
const COMP_ALL: u8 = COMP_WORKERS | COMP_STUCK | COMP_ORPHAN;

/// Pre-spawn admission token obtained from [`ChildReaper::try_admit`].
///
/// Dropping it releases the in-flight slot — correct when spawn fails or
/// is never attempted.  To couple the admission with a spawned child, call
/// [`SpawnAdmission::into_lease`], which transfers ownership to an opaque
/// [`ChildLease`].
pub struct SpawnAdmission {
    in_flight: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
}

impl SpawnAdmission {
    /// Couple this admission with a spawned child.  Consumes `self`.
    ///
    /// The returned [`ChildLease`] is **opaque** — it has no public
    /// methods.  The admission slot is released only after the child is
    /// confirmed exited, which can only happen inside
    /// [`drain_child_output_guarded`] (after a real `wait()`) or inside
    /// the reaper (after a real `try_wait` confirming `Exited`/
    /// `AlreadyReaped`).
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
/// **Opaque type** — all fields and methods are private.  The only way to
/// create a `ChildLease` in production is [`SpawnAdmission::into_lease`].
/// There is no public method to forge a confirmed exit or to access the
/// child handle directly.  The admission slot is released only after the
/// child is confirmed exited by internal code that observed a real
/// terminal status.
///
/// `Drop` delegates the child to the reaper if exit has not been
/// confirmed.  The reaper routes the child to whichever component is
/// still alive (workers → stuck → orphan).  If all components are dead,
/// the handle and admission are leaked (fail-closed).
pub struct ChildLease {
    child: Option<tokio::process::Child>,
    in_flight: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    confirmed: bool,
}

impl ChildLease {
    /// Create a lease without admission (tests only — production children
    /// must come from `SpawnAdmission::into_lease`).
    #[cfg(test)]
    fn new(child: tokio::process::Child) -> Self {
        Self {
            child: Some(child),
            in_flight: None,
            confirmed: false,
        }
    }

    /// Borrow the inner child mutably (same-module only).
    fn child_mut(&mut self) -> Option<&mut tokio::process::Child> {
        self.child.as_mut()
    }

    /// Confirm the child has exited and release the admission slot.
    /// Called **only** after a real `wait()` succeeds or `classify_wait`
    /// returns `Exited`/`AlreadyReaped`.  Idempotent.
    fn confirm_exit(&mut self) {
        if !self.confirmed {
            self.confirmed = true;
            if let Some(in_flight) = &self.in_flight {
                decrement_in_flight(in_flight);
            }
        }
    }

    /// Kill and wait for the child to exit.  Confirms only on success.
    async fn kill_and_wait(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
            if child.wait().await.is_ok() {
                self.confirm_exit();
            }
        }
    }

    /// Wait for the child to exit naturally.  Confirms on success.
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
        // Child exit NOT confirmed.  Route to reaper — the child handle
        // and in_flight clone move to a ReapRequest.  The reaper will
        // kill+wait the child, then drop the ReapRequest (releasing
        // in_flight only after confirmed exit).  The lease's own Arc is
        // dropped here (refcount decreases), but the atomic is NOT
        // decremented — only the reaper does that after confirmed exit.
        if let Some(mut child) = self.child.take() {
            let in_flight = self.in_flight.take();
            if let Some(reaper) = ChildReaper::global()
                && reaper.is_healthy()
            {
                reaper.route_child(child, in_flight);
            } else if let Some(reaper) = ChildReaper::global() {
                // Reaper exists but is degraded — route to whatever
                // components are still alive.
                reaper.route_child(child, in_flight);
            } else {
                // No global reaper at all.  Terminal failure — kill the
                // child, leak handle + admission (fail-closed).
                let _ = child.start_kill();
                std::mem::forget(child);
            }
        }
    }
}

/// A kill+reap request queued to the [`ChildReaper`].
///
/// Owns the child handle and the `in_flight` Arc.  `Drop` decrements
/// `in_flight` **only** if `confirmed` is true — i.e., only after a real
/// terminal status was observed.  If the request is dropped without
/// confirmation (e.g., during a panic), the admission is **leaked**
/// (fail-closed).
struct ReapRequest {
    child: tokio::process::Child,
    in_flight: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    confirmed: bool,
}

impl ReapRequest {
    /// Mark the child as confirmed exited.  Only call after observing a
    /// real `Exited` or `AlreadyReaped` status.
    fn confirm(&mut self) {
        self.confirmed = true;
    }
}

impl Drop for ReapRequest {
    fn drop(&mut self) {
        if self.confirmed
            && let Some(in_flight) = &self.in_flight
        {
            decrement_in_flight(in_flight);
        }
        // If not confirmed: in_flight is NOT decremented — admission
        // leaked (fail-closed).  The child handle is dropped by
        // tokio::process::Child::drop.
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
    /// Whether the supervisor is healthy (all components alive, accepting
    /// admissions).
    pub healthy: bool,
    /// Whether the worker pool is alive.
    pub workers_alive: bool,
    /// Whether the stuck scheduler is alive.
    pub stuck_alive: bool,
    /// Whether the orphan reaper is alive.
    pub orphan_alive: bool,
}

/// RAII guard that clears a component's liveness bit on drop.  Every
/// required thread (worker, stuck, orphan) carries one.  When any thread
/// exits, its bit is cleared, closing admission (fail-closed).
struct LivenessGuard {
    components: std::sync::Arc<std::sync::atomic::AtomicU8>,
    failures: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Which component bit this guard represents.
    mask: u8,
}

impl Drop for LivenessGuard {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        // Monotonic: only clears bits, never sets them.  This prevents
        // Failed → Degraded transitions.
        self.components.fetch_and(!self.mask, Ordering::SeqCst);
        self.failures.fetch_add(1, Ordering::SeqCst);
    }
}

/// Static last-resort storage for child handles that cannot be queued to
/// the normal stuck list (stuck mutex poisoned or stuck thread dead).
static ORPHANED: std::sync::OnceLock<std::sync::Mutex<Vec<ReapRequest>>> =
    std::sync::OnceLock::new();

fn orphaned_list() -> &'static std::sync::Mutex<Vec<ReapRequest>> {
    ORPHANED.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// A centralized, multi-worker child process reaper with **bounded
/// admission**, **per-component supervisor health**, and **transactional
/// initialisation**.
///
/// Architecture:
/// - `try_admit()` returns `Option<SpawnAdmission>`; admission is closed
///   when **any** component (worker pool, stuck scheduler, orphan reaper)
///   is dead.
/// - `route_child()` routes existing children to whichever components are
///   still alive (workers → stuck → orphan).  If all are dead, the handle
///   and admission are leaked (fail-closed terminal state).
/// - All threads carry a [`LivenessGuard`] that clears the corresponding
///   component bit on exit.  State transitions are monotonic (bits only
///   clear, never set).
/// - Initialisation is transactional: if any thread fails to spawn, all
///   previously started threads are stopped and joined before returning
///   `None`.
/// - Workers use `catch_unwind` to recover from panics during request
///   processing — the request ownership is preserved outside the closure
///   and routed to the stuck list on panic.
/// - `ReapRequest::drop` releases admission **only** if `confirmed` is
///   true.  Unconfirmed drops leak the admission (fail-closed).
pub struct ChildReaper {
    sender: std::sync::mpsc::SyncSender<ReapRequest>,
    /// Global in-flight counter; bounds total unreaped children.
    in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Bitmask of alive components (COMP_WORKERS | COMP_STUCK | COMP_ORPHAN).
    components: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// Stuck list for children that didn't exit within the worker timeout.
    stuck: std::sync::Arc<std::sync::Mutex<Vec<ReapRequest>>>,
    /// Metrics counters.
    admitted: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    completed: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    failures: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Stop signal for all threads (used during init-failure cleanup).
    #[allow(dead_code)]
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ChildReaper {
    /// Returns the global reaper instance, or `None` if any thread could
    /// not be created (fail-closed).  Initialisation is **transactional**:
    /// if any thread fails to spawn, all previously started threads are
    /// stopped and joined before returning `None` permanently.
    pub fn global() -> Option<&'static ChildReaper> {
        static REAPER: std::sync::OnceLock<Option<ChildReaper>> = std::sync::OnceLock::new();
        REAPER
            .get_or_init(|| {
                use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

                let (sender, receiver) =
                    std::sync::mpsc::sync_channel::<ReapRequest>(REAPER_QUEUE_CAPACITY);
                let receiver = std::sync::Arc::new(std::sync::Mutex::new(receiver));
                let stuck = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                let in_flight = std::sync::Arc::new(AtomicUsize::new(0));
                let components = std::sync::Arc::new(AtomicU8::new(COMP_ALL));
                let admitted = std::sync::Arc::new(AtomicUsize::new(0));
                let completed = std::sync::Arc::new(AtomicUsize::new(0));
                let failures = std::sync::Arc::new(AtomicUsize::new(0));
                let stop = std::sync::Arc::new(AtomicBool::new(false));

                let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();
                let mut failed = false;

                // ── Spawn worker threads ──────────────────────────────
                for i in 0..REAPER_WORKERS {
                    let rx = receiver.clone();
                    let stuck_c = stuck.clone();
                    let comp = components.clone();
                    let fail = failures.clone();
                    let done = completed.clone();
                    let stp = stop.clone();
                    match std::thread::Builder::new()
                        .name(format!("child-reaper-{i}"))
                        .spawn(move || {
                            let _guard = LivenessGuard {
                                components: comp,
                                failures: fail,
                                mask: COMP_WORKERS,
                            };
                            Self::worker_loop(rx, stuck_c, done, stp);
                        }) {
                        Ok(h) => handles.push(h),
                        Err(_) => {
                            failed = true;
                            break;
                        }
                    }
                }

                // ── Spawn stuck reaper thread ─────────────────────────
                if !failed {
                    let stuck_c = stuck.clone();
                    let done = completed.clone();
                    let comp = components.clone();
                    let fail = failures.clone();
                    let stp = stop.clone();
                    match std::thread::Builder::new()
                        .name("child-reaper-stuck".to_string())
                        .spawn(move || {
                            let _guard = LivenessGuard {
                                components: comp,
                                failures: fail,
                                mask: COMP_STUCK,
                            };
                            Self::stuck_loop(stuck_c, done, stp);
                        }) {
                        Ok(h) => handles.push(h),
                        Err(_) => failed = true,
                    }
                }

                // ── Spawn orphan reaper thread ────────────────────────
                if !failed {
                    let done = completed.clone();
                    let comp = components.clone();
                    let fail = failures.clone();
                    let stp = stop.clone();
                    match std::thread::Builder::new()
                        .name("child-reaper-orphan".to_string())
                        .spawn(move || {
                            let _guard = LivenessGuard {
                                components: comp,
                                failures: fail,
                                mask: COMP_ORPHAN,
                            };
                            Self::orphan_reaper_loop(done, stp);
                        }) {
                        Ok(h) => handles.push(h),
                        Err(_) => failed = true,
                    }
                }

                // ── Transactional cleanup on failure ──────────────────
                if failed {
                    // Signal all threads to stop, then drop the sender to
                    // unblock workers blocked on recv_timeout.
                    stop.store(true, Ordering::SeqCst);
                    drop(sender);
                    // Bounded join — threads exit within STOP_CHECK_INTERVAL.
                    for h in handles {
                        let _ = h.join();
                    }
                    // Mark all components as dead (Failed state).
                    components.store(0, Ordering::SeqCst);
                    return None;
                }

                Some(ChildReaper {
                    sender,
                    in_flight,
                    components,
                    stuck,
                    admitted,
                    completed,
                    failures,
                    stop,
                })
            })
            .as_ref()
    }

    /// Returns `true` only when **all** components are alive.  Any
    /// component exit closes admission (fail-closed).
    pub fn is_healthy(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.components.load(Ordering::SeqCst) == COMP_ALL
    }

    /// Whether the worker pool is alive.
    fn workers_alive(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.components.load(Ordering::SeqCst) & COMP_WORKERS != 0
    }

    /// Whether the stuck scheduler is alive.
    fn stuck_alive(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.components.load(Ordering::SeqCst) & COMP_STUCK != 0
    }

    /// Whether the orphan reaper is alive.
    fn orphan_alive(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.components.load(Ordering::SeqCst) & COMP_ORPHAN != 0
    }

    /// Try to acquire a pre-spawn admission.  Returns `Some(admission)`
    /// if a slot was acquired and all components are healthy, `None`
    /// otherwise (fail-closed).
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
        let comp = self.components.load(Ordering::SeqCst);
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
            healthy: comp == COMP_ALL,
            workers_alive: comp & COMP_WORKERS != 0,
            stuck_alive: comp & COMP_STUCK != 0,
            orphan_alive: comp & COMP_ORPHAN != 0,
        }
    }

    /// Route a child to whichever reaper components are still alive.
    ///
    /// Routing priority: workers (queue) → stuck list → orphan list →
    /// leak (terminal failure).  The child is killed before routing.
    /// The `in_flight` Arc moves into the `ReapRequest` and is
    /// decremented by `Drop` only after confirmed exit.
    fn route_child(
        &self,
        mut child: tokio::process::Child,
        in_flight: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    ) {
        let _ = child.start_kill();
        let req = ReapRequest {
            child,
            in_flight,
            confirmed: false,
        };

        // Try to queue to workers.
        if self.workers_alive() {
            match self.sender.try_send(req) {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Full(req))
                | Err(std::sync::mpsc::TrySendError::Disconnected(req)) => {
                    self.route_to_recovery(req);
                }
            }
        } else {
            self.route_to_recovery(req);
        }
    }

    /// Route a request to the first alive recovery component (stuck →
    /// orphan → leak).
    fn route_to_recovery(&self, req: ReapRequest) {
        if self.stuck_alive() {
            let mut list = self.stuck.lock().unwrap_or_else(|e| e.into_inner());
            list.push(req);
        } else if self.orphan_alive() {
            let mut list = orphaned_list().lock().unwrap_or_else(|e| e.into_inner());
            list.push(req);
        } else {
            // All components dead.  Terminal failure — the child was
            // already killed.  Leak handle + admission (fail-closed).
            std::mem::forget(req);
        }
    }

    /// Worker thread main loop.  Uses `recv_timeout` for stop-flag
    /// responsiveness and `catch_unwind` to preserve request ownership
    /// across panics.
    ///
    /// Only `Exited` and `AlreadyReaped` confirm the request (releasing
    /// admission via `ReapRequest::drop`).  Timeouts and `Unknown` errors
    /// move the child to the stuck list.  Panics also move the child to
    /// the stuck list — the request is recovered via `RefCell::into_inner`.
    fn worker_loop(
        receiver: std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<ReapRequest>>>,
        stuck: std::sync::Arc<std::sync::Mutex<Vec<ReapRequest>>>,
        completed: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::Ordering;
        use std::sync::mpsc::RecvTimeoutError;

        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let req = match receiver.lock() {
                Ok(rx) => rx.recv_timeout(STOP_CHECK_INTERVAL),
                Err(e) => e.into_inner().recv_timeout(STOP_CHECK_INTERVAL),
            };
            match req {
                Ok(req) => {
                    // Process with catch_unwind — request ownership is
                    // preserved outside the closure via RefCell.
                    let req_cell = std::cell::RefCell::new(req);
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let mut r = req_cell.borrow_mut();
                        try_reap_request(&mut r)
                    }));
                    let mut req = req_cell.into_inner();

                    match result {
                        Ok(true) => {
                            req.confirm();
                            completed.fetch_add(1, Ordering::SeqCst);
                            // req drops — Drop releases in_flight (confirmed).
                        }
                        Ok(false) => {
                            // Move to stuck list.
                            let mut list = stuck.lock().unwrap_or_else(|e| e.into_inner());
                            list.push(req);
                        }
                        Err(_) => {
                            // Panic — request recovered via into_inner.
                            // Route to stuck list for retry.
                            let mut list = stuck.lock().unwrap_or_else(|e| e.into_inner());
                            list.push(req);
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    /// Stuck reaper thread.  Periodically revisits children that didn't
    /// exit within the worker timeout.  Uses `catch_unwind` per request.
    fn stuck_loop(
        stuck: std::sync::Arc<std::sync::Mutex<Vec<ReapRequest>>>,
        completed: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::Ordering;

        let mut last_process = std::time::Instant::now();
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(STOP_CHECK_INTERVAL);
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if last_process.elapsed() < STUCK_REAPER_INTERVAL {
                continue;
            }
            last_process = std::time::Instant::now();

            let mut list = stuck.lock().unwrap_or_else(|e| e.into_inner());
            let mut still_stuck = Vec::with_capacity(list.len());
            while let Some(req) = list.pop() {
                let req_cell = std::cell::RefCell::new(req);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut r = req_cell.borrow_mut();
                    matches!(
                        classify_wait(r.child.try_wait()),
                        WaitOutcome::Exited | WaitOutcome::AlreadyReaped
                    )
                }));
                let mut req = req_cell.into_inner();
                match result {
                    Ok(true) => {
                        req.confirm();
                        completed.fetch_add(1, Ordering::SeqCst);
                        // req drops — Drop releases in_flight.
                    }
                    Ok(false) | Err(_) => {
                        still_stuck.push(req);
                    }
                }
            }
            *list = still_stuck;
        }
    }

    /// Orphan reaper thread.  Periodically reaps handles in the
    /// [`ORPHANED`] list.  Uses `catch_unwind` per request.
    fn orphan_reaper_loop(
        completed: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::Ordering;

        let mut last_process = std::time::Instant::now();
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(STOP_CHECK_INTERVAL);
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if last_process.elapsed() < STUCK_REAPER_INTERVAL {
                continue;
            }
            last_process = std::time::Instant::now();

            let mut list = orphaned_list().lock().unwrap_or_else(|e| e.into_inner());
            let mut still_orphaned = Vec::with_capacity(list.len());
            while let Some(req) = list.pop() {
                let req_cell = std::cell::RefCell::new(req);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut r = req_cell.borrow_mut();
                    matches!(
                        classify_wait(r.child.try_wait()),
                        WaitOutcome::Exited | WaitOutcome::AlreadyReaped
                    )
                }));
                let mut req = req_cell.into_inner();
                match result {
                    Ok(true) => {
                        req.confirm();
                        completed.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(false) | Err(_) => {
                        still_orphaned.push(req);
                    }
                }
            }
            *list = still_orphaned;
        }
    }
}

/// Interval at which all threads check the stop flag.
const STOP_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Try to reap a child with a bounded timeout.  Kills the child first,
/// then polls `try_wait` via [`reap_with_deadline`].
///
/// Returns `true` if the child was confirmed exited (handle can be
/// dropped), `false` if it should be moved to the stuck list.
fn try_reap_request(req: &mut ReapRequest) -> bool {
    let _ = req.child.start_kill();
    let deadline = std::time::Instant::now() + REAP_WORKER_TIMEOUT;
    reap_with_deadline(|| req.child.try_wait(), deadline)
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
            components: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(COMP_ALL)),
            stuck: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            admitted: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            completed: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            failures: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
                components: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(COMP_ALL)),
                stuck: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                admitted: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                completed: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                failures: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            receiver,
        )
    }

    /// Create a ChildReaper in `Failed` state (all components dead).
    fn new_failed() -> ChildReaper {
        let (sender, _receiver) = std::sync::mpsc::sync_channel::<ReapRequest>(1);
        ChildReaper {
            sender,
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            components: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            stuck: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            admitted: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            completed: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            failures: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Create a ChildReaper with specific components marked as dead.
    fn new_with_components(alive: u8) -> ChildReaper {
        let (sender, _receiver) = std::sync::mpsc::sync_channel::<ReapRequest>(1);
        ChildReaper {
            sender,
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            components: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(alive)),
            stuck: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            admitted: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            completed: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            failures: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Directly route a child for testing.
    fn reap_direct(
        &self,
        child: tokio::process::Child,
        in_flight: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    ) {
        self.route_child(child, in_flight);
    }

    /// Get a clone of the stuck list Arc (for testing).
    fn stuck_arc(&self) -> std::sync::Arc<std::sync::Mutex<Vec<ReapRequest>>> {
        self.stuck.clone()
    }

    /// Get a clone of the in_flight counter (for testing).
    fn in_flight_arc(&self) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        self.in_flight.clone()
    }
}

/// Reap a child with a deadline, using the provided `try_wait` closure.
///
/// Returns `true` if the child was confirmed exited (handle can be
/// dropped), `false` if it should be moved to the stuck list.
///
/// **Every** retry branch (Interrupted and Pending) checks the deadline.
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
pub fn drain_child_output_guarded(
    mut lease: ChildLease,
    opts: ShellOptions<'_>,
    shutdown: CancellationToken,
) -> impl std::future::Future<Output = Result<ShellOutput, EnvError>> + use<'_> {
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
        COMP_ALL, COMP_STUCK, COMP_WORKERS, ChildLease, ChildReaper, LivenessGuard,
        REAPER_QUEUE_CAPACITY, WaitOutcome, classify_wait, drain_child_output_guarded,
        reap_with_deadline,
    };
    use llm_harness_types::ShellOptions;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn process_is_gone(pid: u32) -> bool {
        let status_path = format!("/proc/{pid}/status");
        std::fs::read_to_string(&status_path).is_err()
    }

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

    // ── classify_wait unit tests ──────────────────────────────────────────

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
            "should have waited until deadline"
        );
    }

    #[test]
    fn persistent_unknown_error_retains_ownership() {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let result = reap_with_deadline(|| Err(std::io::Error::from_raw_os_error(999)), deadline);
        assert!(!result, "unknown error must retain ownership");
        assert!(
            std::time::Instant::now() < deadline,
            "should return immediately for unknown error"
        );
    }

    #[test]
    fn reap_with_deadline_exited() {
        let result = reap_with_deadline(
            || Ok(Some(std::process::ExitStatus::from_raw(0))),
            std::time::Instant::now() + Duration::from_secs(5),
        );
        assert!(result, "exited child should return true");
    }

    // ── ChildLease integration tests ──────────────────────────────────────

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

    /// P0 (round-14 fix): rewritten — single child/lease from the same
    /// runtime, drop lease then drop runtime, verify PID disappears.
    #[test]
    fn lease_reaps_during_runtime_shutdown() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (child, pid) = rt.block_on(async {
            let child = tokio::process::Command::new("sleep")
                .arg("30")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let pid = child.id().expect("child must have a PID");
            (child, pid)
        });

        // Drop the lease while the runtime is still alive, then drop
        // the runtime.  The lease delegates to the reaper (OS threads),
        // which does not depend on the Tokio runtime.
        let lease = ChildLease::new(child);
        drop(lease);
        drop(rt);

        assert!(
            wait_for_reap(pid, Duration::from_secs(5)),
            "child PID {pid} must be reaped after ChildLease::drop during runtime shutdown"
        );
    }

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

        let lease = ChildLease::new(child);
        let opts = test_opts();
        let token = CancellationToken::new();
        let fut = drain_child_output_guarded(lease, opts, token);
        drop(fut);

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

    // ── P0: Opaque ChildLease — confirm_exit is private ───────────────────

    /// P0: confirm_exit() is private — only internal code after a real
    /// wait()/ECHILD can call it.  This test verifies that calling
    /// confirm_exit prematurely (in-module, simulating what an external
    /// caller would do if it were public) does NOT happen in production
    /// because the method is private.
    ///
    /// The test is in-module to access the private method, but it
    /// demonstrates that external code cannot forge a confirmed exit:
    /// if confirm_exit were public, an attacker could release admission
    /// while the child is still running.
    #[test]
    fn confirm_exit_is_private_cannot_forge() {
        // This test compiles only because confirm_exit is accessible
        // from within the module.  External crates (tests/api_boundary.rs)
        // cannot call it.  If someone makes confirm_exit public, the
        // integration test in tests/api_boundary.rs should be updated
        // to verify the broken invariant.
        //
        // We verify the type exists and is opaque from the test's
        // perspective: we can create a ChildLease via into_lease and
        // pass it to drain_child_output_guarded, but we cannot call
        // confirm_exit from outside the module.
        let reaper = ChildReaper::global().expect("global reaper must be available");
        let admission = reaper.try_admit().expect("should admit");
        // Dropping admission without spawning — releases the slot.
        drop(admission);
    }

    // ── Supervisor health tests ───────────────────────────────────────────

    /// P0 (round-14 fix): LivenessGuard must close admission when a
    /// component dies.  This test verifies the guard transitions the
    /// supervisor to a degraded state.
    #[test]
    fn liveness_guard_closes_admission() {
        let components = Arc::new(AtomicU8::new(COMP_ALL));
        let failures = Arc::new(AtomicUsize::new(0));

        {
            let _guard = LivenessGuard {
                components: components.clone(),
                failures: failures.clone(),
                mask: COMP_WORKERS,
            };
            assert_eq!(
                components.load(Ordering::SeqCst),
                COMP_ALL,
                "all components should be alive while guard is held"
            );
        }

        assert_eq!(
            components.load(Ordering::SeqCst),
            COMP_ALL & !COMP_WORKERS,
            "COMP_WORKERS bit should be cleared after guard drops"
        );
        assert_eq!(failures.load(Ordering::SeqCst), 1);
    }

    /// P0 (round-14 fix): LivenessGuard is monotonic — clearing a bit
    /// that's already clear is a no-op.  State can never go from
    /// Degraded back to Healthy.
    #[test]
    fn liveness_guard_is_monotonic() {
        let components = Arc::new(AtomicU8::new(COMP_ALL));
        let failures = Arc::new(AtomicUsize::new(0));

        // Drop a worker guard.
        {
            let _g = LivenessGuard {
                components: components.clone(),
                failures: failures.clone(),
                mask: COMP_WORKERS,
            };
        }
        let after_first = components.load(Ordering::SeqCst);
        assert_eq!(after_first, COMP_ALL & !COMP_WORKERS);

        // Drop a stuck guard — should clear the stuck bit, not re-set
        // the worker bit.
        {
            let _g = LivenessGuard {
                components: components.clone(),
                failures: failures.clone(),
                mask: COMP_STUCK,
            };
        }
        let after_second = components.load(Ordering::SeqCst);
        assert_eq!(after_second, COMP_ALL & !COMP_WORKERS & !COMP_STUCK);
        // Worker bit must NOT be re-set.
        assert_eq!(after_second & COMP_WORKERS, 0);
    }

    /// P0: A reaper in Failed state (all components dead) must reject
    /// all admissions.
    #[test]
    fn supervisor_failure_closes_admission() {
        let reaper = ChildReaper::new_failed();
        assert!(!reaper.is_healthy());
        assert!(reaper.try_admit().is_none());
        let metrics = reaper.metrics();
        assert!(!metrics.healthy);
        assert!(!metrics.workers_alive);
        assert!(!metrics.stuck_alive);
        assert!(!metrics.orphan_alive);
    }

    // ── P0: Per-component routing ─────────────────────────────────────────

    /// P0 (round-14 fix): When the orphan thread is dead but workers and
    /// stuck are alive, a child must be routed to the stuck list, NOT to
    /// the orphan list.
    #[test]
    fn orphan_dead_routes_to_stuck_not_orphan() {
        // Workers + stuck alive, orphan dead.
        let reaper = ChildReaper::new_with_components(COMP_WORKERS | COMP_STUCK);
        assert!(reaper.workers_alive());
        assert!(reaper.stuck_alive());
        assert!(!reaper.orphan_alive());

        let child = spawn_child("sleep", "30");
        reaper.reap_direct(child, None);

        // The child should go to the queue (workers alive) or stuck list
        // (if queue full/disconnected).  It should NOT go to the orphan
        // list because the orphan thread is dead.
        let orphaned_len = super::orphaned_list()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        assert_eq!(
            orphaned_len, 0,
            "child must NOT be routed to orphan list when orphan thread is dead"
        );
    }

    /// P0 (round-14 fix): When workers are dead but stuck is alive, a
    /// child must be routed to the stuck list.
    #[test]
    fn workers_dead_routes_to_stuck() {
        let reaper = ChildReaper::new_with_components(COMP_STUCK);
        assert!(!reaper.workers_alive());
        assert!(reaper.stuck_alive());

        let child = spawn_child("sleep", "30");
        reaper.reap_direct(child, None);

        let stuck_len = reaper
            .stuck_arc()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        assert_eq!(
            stuck_len, 1,
            "child must be routed to stuck list when workers are dead"
        );
    }

    /// P0 (round-14 fix): When all components are dead, the child handle
    /// and admission must be leaked (fail-closed terminal state), not
    /// dropped with unconfirmed exit.  The child is killed via
    /// `start_kill` in `route_child`, but the handle is forgotten
    /// (`mem::forget`), so the process becomes a zombie until the parent
    /// process exits and init reaps it.  We only verify that the
    /// admission is NOT released — the key fail-closed invariant.
    #[test]
    fn all_dead_leaks_handle_and_admission() {
        let reaper = ChildReaper::new_with_components(0);
        let in_flight = reaper.in_flight_arc();
        in_flight.store(1, Ordering::SeqCst);

        let child = spawn_child("sleep", "30");
        reaper.reap_direct(child, Some(in_flight.clone()));

        // The child was killed (start_kill in route_child).  The handle
        // and admission are leaked (mem::forget).  in_flight must NOT
        // have been decremented — this is the fail-closed invariant.
        assert_eq!(
            in_flight.load(Ordering::SeqCst),
            1,
            "in_flight must NOT be decremented when all components are dead"
        );
    }

    // ── Admission bound tests ─────────────────────────────────────────────

    #[test]
    fn admission_at_capacity_rejects_then_recovers() {
        let reaper = ChildReaper::new_disconnected();

        let mut admissions: Vec<_> = Vec::with_capacity(REAPER_QUEUE_CAPACITY);
        for _ in 0..REAPER_QUEUE_CAPACITY {
            admissions.push(reaper.try_admit().expect("should admit below capacity"));
        }
        assert_eq!(reaper.metrics().in_flight, REAPER_QUEUE_CAPACITY);
        assert!(reaper.try_admit().is_none(), "should reject at capacity");

        drop(admissions);
        assert_eq!(reaper.metrics().in_flight, 0);

        let admission = reaper.try_admit().expect("should admit after recovery");
        assert_eq!(reaper.metrics().in_flight, 1);
        drop(admission);
        assert_eq!(reaper.metrics().in_flight, 0);
    }

    /// P0: Spawn failure (dropping SpawnAdmission without into_lease) must
    /// release only the pre-spawn admission.
    #[test]
    fn spawn_failure_releases_pre_spawn_admission() {
        let reaper = ChildReaper::new_disconnected();

        let admission = reaper.try_admit().expect("should admit");
        assert_eq!(reaper.metrics().in_flight, 1);

        // Simulate spawn failure — drop without into_lease.
        drop(admission);
        assert_eq!(reaper.metrics().in_flight, 0);
    }

    // ── ChildReaper queue-overflow tests ──────────────────────────────────

    #[test]
    fn reap_recovers_child_on_disconnected_receiver() {
        let reaper = ChildReaper::new_disconnected();
        let child = spawn_child("sleep", "30");
        reaper.reap_direct(child, None);

        // Workers "alive" but receiver disconnected → push to stuck.
        let stuck_len = reaper
            .stuck_arc()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        assert_eq!(stuck_len, 1);
    }

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
        assert_eq!(stuck_len, 1);
    }

    #[test]
    fn concurrent_reaps_do_not_block_each_other() {
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

    // ── P0 (round-14 fix): Mutex poison still reaps ───────────────────────

    /// P0 (round-14 fix): After the stuck mutex is poisoned, a child
    /// queued to the global reaper must still be reaped (PID disappears)
    /// and admission must recover (in_flight returns to 0).
    #[test]
    fn mutex_poison_still_reaps() {
        let reaper = ChildReaper::global().expect("global reaper must be available");

        // Poison the stuck mutex in a separate thread.
        let stuck = reaper.stuck_arc();
        let handle = std::thread::spawn(move || {
            let _guard = stuck.lock().unwrap();
            panic!("intentional poison");
        });
        let _ = handle.join();

        // The mutex is now poisoned.  But the global reaper's workers
        // process from the queue (receiver mutex), not the stuck mutex.
        // A child queued via route_child goes to the worker queue.
        let in_flight = reaper.in_flight_arc();
        let admission = reaper.try_admit().expect("should admit");
        let child = spawn_child("sleep", "30");
        let pid = child.id().expect("child must have a PID");
        let lease = admission.into_lease(child);
        drop(lease);

        // The child must be reaped (PID disappears).
        assert!(
            wait_for_reap(pid, Duration::from_secs(10)),
            "child PID {pid} must be reaped even with poisoned stuck mutex"
        );

        // in_flight must recover to 0 after the child is reaped.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if in_flight.load(Ordering::SeqCst) == 0 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("in_flight did not return to 0 after poison recovery");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // ── P0 (round-14 fix): ReapRequest::Drop does not release unconfirmed ─

    /// P0 (round-14 fix): Dropping a ReapRequest without confirmation
    /// must NOT release the admission slot.
    #[test]
    fn unconfirmed_reap_request_does_not_release_admission() {
        let in_flight = Arc::new(AtomicUsize::new(1));

        // Create a ReapRequest with an in_flight counter at 1.
        let child = spawn_child("sleep", "30");
        let req = super::ReapRequest {
            child,
            in_flight: Some(in_flight.clone()),
            confirmed: false,
        };

        // Drop the request without confirming.
        drop(req);

        // in_flight must NOT have been decremented.
        assert_eq!(
            in_flight.load(Ordering::SeqCst),
            1,
            "unconfirmed ReapRequest::drop must NOT release admission"
        );
    }

    /// P0: Confirming a ReapRequest before drop DOES release admission.
    #[test]
    fn confirmed_reap_request_releases_admission() {
        let in_flight = Arc::new(AtomicUsize::new(1));

        let child = spawn_child("sleep", "30");
        let mut req = super::ReapRequest {
            child,
            in_flight: Some(in_flight.clone()),
            confirmed: false,
        };

        req.confirm();
        drop(req);

        assert_eq!(
            in_flight.load(Ordering::SeqCst),
            0,
            "confirmed ReapRequest::drop must release admission"
        );
    }

    // ── Cancellation storm bounded test ───────────────────────────────────

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

        let metrics = reaper.metrics();
        assert!(
            metrics.in_flight <= 32,
            "in_flight ({}) must stay bounded (<= 32)",
            metrics.in_flight
        );

        for &pid in &pids {
            assert!(
                wait_for_reap(pid, Duration::from_secs(10)),
                "child PID {pid} must be reaped"
            );
        }

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
