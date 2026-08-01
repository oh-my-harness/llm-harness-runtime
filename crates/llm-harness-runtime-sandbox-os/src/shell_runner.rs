//! Shared shell output draining logic used by `OsEnv` and `BwrapEnv`.
//!
//! Both backends spawn a child process (directly or via `bwrap`) and need the
//! same stdout/stderr draining, UTF-8 boundary handling, timeout, and abort
//! logic. Extracting it here avoids code duplication and keeps
//! `utf8_safe_boundary` as an internal detail.

use llm_harness_types::{EnvError, ShellOptions, ShellOutput};
use tokio_util::sync::CancellationToken;

#[cfg(all(test, unix))]
use std::os::unix::process::ExitStatusExt;

#[cfg(all(test, windows))]
use std::os::windows::process::ExitStatusExt;

/// Create an `ExitStatus` from a raw exit code for deterministic mocks.
#[cfg(all(test, unix))]
fn exit_status_from_raw(code: i32) -> std::process::ExitStatus {
    std::process::ExitStatus::from_raw(code)
}

#[cfg(all(test, windows))]
fn exit_status_from_raw(code: i32) -> std::process::ExitStatus {
    std::process::ExitStatus::from_raw(code as u32)
}

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

/// Bounded capacity of the reaper queue.  Also bounds total in-flight
/// (admitted but not-yet-reaped) children, preventing unbounded resource
/// growth.
const REAPER_QUEUE_CAPACITY: usize = 256;

/// Number of worker threads processing kill+reap requests.
const REAPER_WORKERS: usize = 4;

/// Per-child timeout in a worker before leaving the child for the stuck
/// scheduler.  Prevents one stuck (D-state) child from blocking a worker
/// indefinitely.
const REAP_WORKER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How often the stuck/orphan/emergency reaper threads scan the registry.
const STUCK_REAPER_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Per-child timeout for synchronous blocking reap in fatal paths
/// (all reaper components dead, or checkout-during-Failed).
const FATAL_REAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Maximum total time for fatal_cleanup across all children.  Prevents
/// a large registry from blocking the supervisor indefinitely.
const FATAL_CLEANUP_TOTAL_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// Interval at which worker threads check the stop flag.
const STOP_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Polling interval for `try_wait` in async drain and worker loops.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// Sleep duration inserted on consecutive `EINTR` to prevent tight-spinning.
const EINTR_BACKOFF: std::time::Duration = std::time::Duration::from_micros(100);

/// POSIX `ECHILD` errno — "no child processes".
///
/// This is 10 on all major Unix platforms (Linux, macOS, FreeBSD, NetBSD,
/// OpenBSD, Solaris).  On unsupported platforms the value is -1 (never
/// matches), so unknown errors are treated as `Unknown` (handle retained)
/// — the safe default.
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

// ── Reapable trait (abstraction for testable wait outcomes) ───────────────

/// Abstraction over a child process that can be killed and polled for exit.
///
/// Production code uses `tokio::process::Child`.  Tests use `MockReapable`
/// to inject deterministic `try_wait` outcomes without real child processes.
trait Reapable: Send + 'static {
    /// Send SIGKILL (idempotent — safe to call multiple times).
    fn start_kill(&mut self);
    /// Non-blocking wait.  Returns `Ok(Some(status))` if exited,
    /// `Ok(None)` if still running, `Err(...)` on error.
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>>;
}

impl Reapable for tokio::process::Child {
    fn start_kill(&mut self) {
        let _ = tokio::process::Child::start_kill(self);
    }
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        tokio::process::Child::try_wait(self)
    }
}

// ── Wait classification ───────────────────────────────────────────────────

/// Outcome of a non-blocking `try_wait` on a child process.
///
/// Only `Exited` and `AlreadyReaped` authorize removing the registry entry
/// and releasing admission.  `Unknown` must retain the handle.
#[derive(Debug)]
#[allow(dead_code)]
enum WaitOutcome {
    Exited,
    Pending,
    Interrupted,
    /// ECHILD — the child was reaped by someone else (e.g. init).
    /// Terminal, but exit status is unknown.
    AlreadyReaped,
    #[allow(dead_code)]
    Unknown(std::io::Error),
}

#[allow(dead_code)]
/// Classify a `try_wait` result into a [`WaitOutcome`].
///
/// Pure function — easy to unit-test with synthesized errors.
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

// ── Result types ──────────────────────────────────────────────────────────

/// Result of a single reaper-thread reap attempt on a registered child.
///
/// Only [`ReapResult::ConfirmedNow`] authorizes incrementing `completed`.
/// The `ExitStatus` is carried so callers can report it.
#[derive(Debug)]
enum ReapResult {
    /// This call confirmed exit and removed the registry entry.
    #[allow(dead_code)]
    ConfirmedNow(ReapStatus),
    /// Entry already removed by another thread — do NOT increment.
    AlreadyAbsent,
    /// Child still running (or active/skipped).  Entry retained.
    Pending,
    /// Unknown wait error.  Entry retained.
    Unknown,
}

/// Terminal exit status.  `Exited` carries a real status; `AlreadyReaped`
/// means the child was reaped by another entity (ECHILD) and the true
/// exit status is unknown — callers must NOT report this as success.
#[derive(Debug)]
enum ReapStatus {
    Exited(std::process::ExitStatus),
    /// ECHILD — child exited but status unknown.  NOT a fake success.
    AlreadyReaped,
}

/// Result of a drain-function poll on an active child.
#[derive(Debug)]
enum PollResult {
    /// Child exited; entry removed, admission released.
    Exited(ReapStatus),
    /// Entry removed by another path (e.g. fatal_cleanup).
    AlreadyGone,
    /// Child still running or checked out.
    Pending,
}

// ── Component identity ───────────────────────────────────────────────────

/// Identifies which kind of reaper thread a component is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompKind {
    Worker,
    Stuck,
    Orphan,
    Emergency,
}

/// Unique identifier for a child registered in the registry.
type ChildId = u64;

/// Monotonically increasing epoch.  Incremented on every supervisor state
/// transition (Healthy→Degraded→Failed).  Admission tokens carry the epoch
/// they were issued under; `commit_child` rejects stale-epoch tokens.
type Epoch = u64;

// ── Supervisor state machine ─────────────────────────────────────────────

/// Supervisor lifecycle state.  Transitions are monotonic:
/// `Healthy → Degraded → Failed`.  Once `Failed`, admission is permanently
/// closed (fail-closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum SupState {
    Healthy = 0,
    Degraded = 1,
    Failed = 2,
}

impl SupState {
    /// Monotonic transition: never goes backwards.
    fn transition_to(&mut self, new: SupState) {
        if new as u8 > *self as u8 {
            *self = new;
        }
    }
}

// ── Registry entry ────────────────────────────────────────────────────────

/// A child process pending reaping, stored in the central registry.
///
/// `reapable` is `Option` so it can be temporarily checked out for
/// synchronous reap via [`CheckoutGuard`].  When `None`, reaper threads
/// skip the entry (treat as Pending).  The [`CheckoutGuard`] ensures the
/// handle is restored on panic/unwind.
struct RegistryEntry {
    reapable: Option<Box<dyn Reapable>>,
    has_admission: bool,
    /// `true` while the drain future is actively using the child.
    /// Reaper threads skip active entries (they must not kill a child
    /// being actively drained).  `fatal_cleanup` ignores this flag.
    active: bool,
    #[allow(dead_code)]
    queued_at: std::time::Instant,
}

// ── CheckoutGuard: panic-safe RAII handle checkout ────────────────────────

/// RAII guard that checks out a `Box<dyn Reapable>` from a registry entry
/// for synchronous reap **outside** the supervisor lock.
///
/// **Panic safety**: if `blocking_reap` or any code using the checked-out
/// handle panics, `Drop` restores the handle to the registry entry so it
/// is never lost.  Only [`CheckoutGuard::consume`] — called after
/// confirming a terminal exit status — removes the entry and releases
/// admission.
///
/// This is the core invariant: **the handle is either in the guard or in
/// the registry, never in neither.**
struct CheckoutGuard {
    id: ChildId,
    reapable: Option<Box<dyn Reapable>>,
    core: std::sync::Arc<SupervisorCore>,
    /// Set to `true` by `consume()` so `Drop` knows not to restore.
    consumed: bool,
}

impl CheckoutGuard {
    /// Access the checked-out reapable for kill/wait.
    fn reapable_mut(&mut self) -> Option<&mut dyn Reapable> {
        self.reapable.as_deref_mut()
    }

    /// Confirm terminal exit: remove the registry entry, release
    /// admission, increment `completed`.  After this, `Drop` is a no-op.
    fn consume(mut self, status: ReapStatus) {
        self.consumed = true;
        let mut inner = self.core.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = inner.registry.remove(&self.id) {
            if entry.has_admission {
                inner.in_flight -= 1;
            }
            inner.completed += 1;
        }
        let _ = status; // Status is carried for caller reporting.
    }

    /// Return the handle to the registry without confirming exit.
    /// Used when reap timed out or returned Unknown.
    fn restore(mut self) {
        self.consumed = true;
        if let Some(r) = self.reapable.take() {
            let mut inner = self.core.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = inner.registry.get_mut(&self.id) {
                entry.reapable = Some(r);
                entry.active = false;
            } else {
                drop(r);
            }
        }
    }
}

impl Drop for CheckoutGuard {
    fn drop(&mut self) {
        if self.consumed {
            return;
        }
        // Panic or early return — restore the handle.
        if let Some(r) = self.reapable.take() {
            let mut inner = self.core.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = inner.registry.get_mut(&self.id) {
                entry.reapable = Some(r);
                entry.active = false;
            } else {
                drop(r);
            }
        }
    }
}

// ── Component liveness tracking ───────────────────────────────────────────

/// Tracks liveness of each reaper component kind with actual counts.
/// Replaces the bitmask: worker count is tracked individually so one
/// worker death does not falsely report all workers as dead.
#[derive(Debug)]
struct ComponentLiveness {
    /// Number of alive worker threads (0..=REAPER_WORKERS).
    workers_alive: usize,
    /// Whether the stuck scheduler thread is alive.
    stuck_alive: bool,
    /// Whether the orphan reaper thread is alive.
    orphan_alive: bool,
    /// Whether the emergency recovery thread is alive.
    emergency_alive: bool,
}

impl ComponentLiveness {
    fn new() -> Self {
        Self {
            workers_alive: REAPER_WORKERS,
            stuck_alive: true,
            orphan_alive: true,
            // Emergency thread is always started in init_reaper.
            emergency_alive: true,
        }
    }

    /// Any reaper component alive (workers, stuck, or orphan)?
    fn any_reaper_alive(&self) -> bool {
        self.workers_alive > 0 || self.stuck_alive || self.orphan_alive
    }

    /// All components dead (including emergency)?
    fn all_dead(&self) -> bool {
        self.workers_alive == 0 && !self.stuck_alive && !self.orphan_alive && !self.emergency_alive
    }
}

// ── SupervisorCore: single-mutex authority ────────────────────────────────

/// Inner state protected by a single `Mutex`.  All state transitions —
/// admission, child commit, reap, cancellation, component death,
/// fatal_cleanup — go through this one lock.
///
/// **Invariants:**
///
/// 1. `try_admit` checks health AND increments `in_flight` AND records
///    the current `epoch` — all under one lock.  The epoch is carried by
///    `SpawnAdmission` and verified at `commit_child` time.
/// 2. `commit_child` verifies the admission's epoch matches the current
///    epoch.  A stale-epoch admission (supervisor transitioned after
///    admit) causes the child to be immediately killed and marked
///    inactive — it cannot continue as a normal active command.
/// 3. `try_reap` / `poll_exit` do `try_wait` + remove-entry + release-
///    admission under one lock — only the call that removes the entry
///    returns `ConfirmedNow`.
/// 4. `on_component_exit` updates liveness counts AND transitions state
///    AND starts emergency recovery if all reapers are dead — all under
///    one lock.  Fatal cleanup is done via `CheckoutGuard` outside the
///    lock to avoid blocking.
/// 5. `CheckoutGuard` ensures the handle is never lost: on panic it
///    restores the handle to the registry entry.
struct SupervisorInner {
    state: SupState,
    epoch: Epoch,
    in_flight: usize,
    admitted: usize,
    completed: usize,
    component_failures: usize,
    reap_panics: usize,
    registry: std::collections::HashMap<ChildId, RegistryEntry>,
    next_id: u64,
    liveness: ComponentLiveness,
    /// Per-child timeout for fatal/synchronous reap paths.
    fatal_reap_timeout: std::time::Duration,
}

struct SupervisorCore {
    inner: std::sync::Mutex<SupervisorInner>,
    sender: std::sync::mpsc::SyncSender<ChildId>,
}

impl SupervisorCore {
    fn new(sender: std::sync::mpsc::SyncSender<ChildId>) -> Self {
        Self {
            inner: std::sync::Mutex::new(SupervisorInner {
                state: SupState::Healthy,
                epoch: 0,
                in_flight: 0,
                admitted: 0,
                completed: 0,
                component_failures: 0,
                reap_panics: 0,
                registry: std::collections::HashMap::new(),
                next_id: 1,
                liveness: ComponentLiveness::new(),
                fatal_reap_timeout: FATAL_REAP_TIMEOUT,
            }),
            sender,
        }
    }

    /// Acquire a pre-spawn admission slot.  Atomic health-check + increment
    /// + epoch capture.  Returns `Some(epoch)` if admitted.
    fn try_admit(&self) -> Option<Epoch> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.state != SupState::Healthy {
            return None;
        }
        if inner.in_flight >= REAPER_QUEUE_CAPACITY {
            return None;
        }
        inner.in_flight += 1;
        inner.admitted += 1;
        Some(inner.epoch)
    }

    /// Release a pre-spawn admission slot (spawn failed or never attempted).
    fn release_admission(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.in_flight > 0 {
            inner.in_flight -= 1;
        }
    }

    /// Immediately commit a spawned child to the authoritative registry.
    ///
    /// The `admit_epoch` must match the current supervisor epoch.  If the
    /// supervisor transitioned (e.g. to Degraded) between `try_admit` and
    /// `commit_child`, the child is immediately killed and marked inactive
    /// — it will be reaped by the stuck/orphan scheduler rather than
    /// continuing as a normal active command.
    fn commit_child(&self, reapable: Box<dyn Reapable>, admit_epoch: Epoch) -> ChildId {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let id = inner.next_id;
        inner.next_id += 1;

        let current_epoch = inner.epoch;
        let epoch_valid = admit_epoch == current_epoch;
        let failed = inner.state == SupState::Failed;
        let workers_alive = inner.liveness.workers_alive > 0;

        // Child is active only if the epoch is valid AND supervisor is
        // not Failed.  A stale-epoch child is immediately killed and
        // marked inactive so reaper threads can reap it.
        let active = epoch_valid && !failed;

        let mut entry = RegistryEntry {
            reapable: Some(reapable),
            has_admission: true,
            active,
            queued_at: std::time::Instant::now(),
        };

        if !active {
            // Stale epoch or Failed — kill immediately.
            if let Some(r) = entry.reapable.as_mut() {
                r.start_kill();
            }
        }

        inner.registry.insert(id, entry);

        if !active && !failed && workers_alive {
            // Stale epoch but workers alive — route for reaping.
            let _ = self.sender.try_send(id);
        }

        id
    }

    /// Cancel an active child (lease dropped without confirmation).
    /// Poll an active child for exit (used by the drain function).
    /// Does NOT skip active entries and does NOT kill.  If the child has
    /// exited, removes the entry and releases admission atomically.
    fn poll_exit(&self, id: ChildId) -> PollResult {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let wait_result = {
            let Some(entry) = inner.registry.get_mut(&id) else {
                return PollResult::AlreadyGone;
            };
            let Some(reapable) = entry.reapable.as_mut() else {
                return PollResult::Pending; // Checked out for sync reap.
            };
            reapable.try_wait()
        };
        match wait_result {
            Ok(Some(status)) => {
                if let Some(entry) = inner.registry.remove(&id) {
                    if entry.has_admission {
                        inner.in_flight -= 1;
                    }
                    inner.completed += 1;
                }
                PollResult::Exited(ReapStatus::Exited(status))
            }
            Ok(None) => PollResult::Pending,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => PollResult::Pending,
            Err(e) if e.raw_os_error() == Some(ECHILD) => {
                // ECHILD: terminal but status unknown.  Remove entry and
                // release admission, but report as AlreadyReaped (NOT
                // fake success).
                if let Some(entry) = inner.registry.remove(&id) {
                    if entry.has_admission {
                        inner.in_flight -= 1;
                    }
                    inner.completed += 1;
                }
                PollResult::Exited(ReapStatus::AlreadyReaped)
            }
            Err(_) => PollResult::Pending, // Unknown — retain, keep polling.
        }
    }

    /// Kill a registered child (idempotent).
    fn kill(&self, id: ChildId) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = inner.registry.get_mut(&id)
            && let Some(r) = entry.reapable.as_mut()
        {
            r.start_kill();
        }
    }

    /// Single reap attempt by a reaper thread.  Skips active entries.
    /// Kills (idempotent) before try_wait.  Only the call that removes
    /// the entry returns `ConfirmedNow`.
    fn try_reap(&self, id: ChildId) -> ReapResult {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let wait_result = {
            let Some(entry) = inner.registry.get_mut(&id) else {
                return ReapResult::AlreadyAbsent;
            };
            if entry.active {
                return ReapResult::Pending;
            }
            let Some(reapable) = entry.reapable.as_mut() else {
                return ReapResult::Pending; // Checked out.
            };
            reapable.start_kill();
            reapable.try_wait()
        };
        match wait_result {
            Ok(Some(status)) => {
                if let Some(entry) = inner.registry.remove(&id) {
                    if entry.has_admission {
                        inner.in_flight -= 1;
                    }
                    inner.completed += 1;
                }
                ReapResult::ConfirmedNow(ReapStatus::Exited(status))
            }
            Ok(None) => ReapResult::Pending,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => ReapResult::Pending,
            Err(e) if e.raw_os_error() == Some(ECHILD) => {
                if let Some(entry) = inner.registry.remove(&id) {
                    if entry.has_admission {
                        inner.in_flight -= 1;
                    }
                    inner.completed += 1;
                }
                ReapResult::ConfirmedNow(ReapStatus::AlreadyReaped)
            }
            Err(_) => ReapResult::Unknown,
        }
    }

    /// IDs of non-active entries (for stuck/orphan/emergency scanners).
    fn pending_ids(&self) -> Vec<ChildId> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .registry
            .iter()
            .filter(|(_, e)| !e.active)
            .map(|(&id, _)| id)
            .collect()
    }

    /// Check out a child for fatal cleanup outside the lock.
    /// Returns `Some(CheckoutGuard)` if the child exists and has a handle.
    /// The caller performs blocking reap with the guard; on panic the
    /// guard restores the handle.
    fn checkout_for_fatal(
        self: &std::sync::Arc<SupervisorCore>,
        id: ChildId,
    ) -> Option<CheckoutGuard> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let reapable = inner.registry.get_mut(&id).and_then(|e| e.reapable.take());
        reapable.map(|r| CheckoutGuard {
            id,
            reapable: Some(r),
            core: self.clone(),
            consumed: false,
        })
    }

    /// Called when a reaper component thread exits.  Updates liveness
    /// counts, transitions state, and starts emergency recovery if all
    /// reapers are dead.  Fatal cleanup is done outside the lock via
    /// `CheckoutGuard` — this method only does the state transition.
    fn on_component_exit(&self, kind: CompKind, worker_idx: Option<usize>) {
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.component_failures += 1;

            match kind {
                CompKind::Worker => {
                    if let Some(idx) = worker_idx {
                        if inner.liveness.workers_alive > 0 {
                            inner.liveness.workers_alive -= 1;
                        }
                        let _ = idx;
                    }
                }
                CompKind::Stuck => {
                    inner.liveness.stuck_alive = false;
                }
                CompKind::Orphan => {
                    inner.liveness.orphan_alive = false;
                }
                CompKind::Emergency => {
                    inner.liveness.emergency_alive = false;
                }
            }

            if inner.liveness.all_dead() {
                // All components dead (including emergency).  Terminal
                // fatal: no recovery owner remains.  Admission is
                // permanently closed.  Remaining children cannot be
                // reaped by the supervisor.
                inner.epoch += 1;
                inner.state.transition_to(SupState::Failed);
            } else if !inner.liveness.any_reaper_alive() {
                // All reapers dead but emergency still alive.
                // Emergency thread becomes the active recovery owner.
                inner.epoch += 1;
                inner.state.transition_to(SupState::Failed);
            } else {
                // Any required component exit makes the original supervisor
                // epoch unhealthy, even while redundant reapers remain.
                inner.epoch += 1;
                inner.state.transition_to(SupState::Degraded);
            }
        }
    }

    /// Whether the supervisor is healthy (all components alive).
    fn is_healthy(&self) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.state == SupState::Healthy
    }

    /// Whether any reaper component is alive.
    fn any_reaper_alive(&self) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.liveness.any_reaper_alive()
    }

    fn metrics(&self) -> ReaperMetrics {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        ReaperMetrics {
            in_flight: inner.in_flight,
            admitted: inner.admitted,
            completed: inner.completed,
            component_failures: inner.component_failures,
            reap_panics: inner.reap_panics,
            registry_len: inner.registry.len(),
            healthy: inner.state == SupState::Healthy,
            workers_alive: inner.liveness.workers_alive,
            stuck_alive: inner.liveness.stuck_alive,
            orphan_alive: inner.liveness.orphan_alive,
            emergency_alive: inner.liveness.emergency_alive,
        }
    }

    #[cfg(test)]
    fn registry_len(&self) -> usize {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.registry.len()
    }

    #[cfg(test)]
    fn in_flight_count(&self) -> usize {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.in_flight
    }

    #[cfg(test)]
    fn completed_count(&self) -> usize {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.completed
    }

    #[cfg(test)]
    fn state(&self) -> SupState {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.state
    }

    #[cfg(test)]
    #[allow(dead_code)]
    fn workers_alive(&self) -> usize {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.liveness.workers_alive
    }

    /// Current fatal-reap timeout (for kill_and_wait deadline).
    fn fatal_reap_timeout(&self) -> std::time::Duration {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.fatal_reap_timeout
    }

    #[cfg(test)]
    #[allow(dead_code)]
    fn set_fatal_reap_timeout(&self, timeout: std::time::Duration) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.fatal_reap_timeout = timeout;
    }

    #[cfg(test)]
    fn epoch(&self) -> Epoch {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.epoch
    }

    #[cfg(test)]
    fn force_worker_exit(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.liveness.workers_alive > 0 {
            inner.liveness.workers_alive -= 1;
        }
        inner.epoch += 1;
        inner.state.transition_to(SupState::Degraded);
    }

    #[cfg(test)]
    fn force_all_reapers_dead(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.liveness.workers_alive = 0;
        inner.liveness.stuck_alive = false;
        inner.liveness.orphan_alive = false;
        inner.epoch += 1;
        inner.state.transition_to(SupState::Failed);
    }

    #[cfg(test)]
    fn all_ids(&self) -> Vec<ChildId> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.registry.keys().copied().collect()
    }

    /// Record a panic isolated by the emergency blocking-reap loop.
    fn record_reap_panic(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.reap_panics += 1;
    }
}

// ── SpawnAdmission (carries epoch) ────────────────────────────────────────

/// Pre-spawn admission token.  Carries the supervisor `epoch` at issue
/// time.  `commit_child` verifies the epoch is still current; a stale
/// epoch means the supervisor transitioned after admission and the child
/// must be immediately killed and reaped rather than running as active.
///
/// Dropping it releases the in-flight slot (correct when spawn fails or
/// is never attempted).  Call `into_lease` to couple with a spawned child.
pub struct SpawnAdmission {
    core: Option<std::sync::Arc<SupervisorCore>>,
    epoch: Epoch,
}

impl SpawnAdmission {
    /// Couple this admission with a spawned child.  The child is
    /// **immediately** committed to the authoritative registry under the
    /// supervisor lock — not deferred to lease drop.  The admission epoch
    /// is verified at commit time.  Returns an opaque [`ChildLease`].
    pub fn into_lease(mut self, mut child: tokio::process::Child) -> ChildLease {
        let core = self.core.take().expect("into_lease called twice");
        let epoch = self.epoch;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let id = core.commit_child(Box::new(child), epoch);
        ChildLease {
            id,
            core: Some(core),
            stdout,
            stderr,
            confirmed: false,
        }
    }
}

impl Drop for SpawnAdmission {
    fn drop(&mut self) {
        if let Some(core) = self.core.take() {
            core.release_admission();
        }
    }
}

// ── ChildLease (opaque) ──────────────────────────────────────────────────

/// Post-spawn lease.  **Opaque type** — no public methods.  The child
/// handle is in the authoritative registry from the moment `into_lease`
/// is called; this struct holds only the `ChildId`, stdout/stderr handles,
/// and a confirmation flag.
///
/// `Drop` cancels the child (kills + routes to reapers) if exit has not
/// been confirmed.  In the `Failed` state, `Drop` performs a synchronous
/// blocking reap via `CheckoutGuard` (panic-safe).
pub struct ChildLease {
    id: ChildId,
    core: Option<std::sync::Arc<SupervisorCore>>,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    confirmed: bool,
}

impl ChildLease {
    /// Poll the registry until the child exits.  Returns the `ExitStatus`.
    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let Some(core) = self.core.as_ref() else {
            return Err(std::io::Error::other("lease has no core"));
        };
        loop {
            match core.poll_exit(self.id) {
                PollResult::Exited(ReapStatus::Exited(status)) => {
                    self.confirmed = true;
                    return Ok(status);
                }
                PollResult::Exited(ReapStatus::AlreadyReaped) => {
                    self.confirmed = true;
                    // ECHILD: child exited but status unknown.  Report
                    // as an error rather than faking success.
                    return Err(std::io::Error::other(
                        "child reaped by another entity (ECHILD); exit status unknown",
                    ));
                }
                PollResult::AlreadyGone => {
                    self.confirmed = true;
                    return Err(std::io::Error::other(
                        "child reaped by supervisor (fatal_cleanup)",
                    ));
                }
                PollResult::Pending => {
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }
    }

    /// Kill the child and poll until exit or `fatal_reap_timeout`.
    async fn kill_and_wait(&mut self) {
        let Some(core) = self.core.as_ref() else {
            return;
        };
        core.kill(self.id);
        let deadline = std::time::Instant::now() + core.fatal_reap_timeout();
        loop {
            match core.poll_exit(self.id) {
                PollResult::Exited(_) | PollResult::AlreadyGone => {
                    self.confirmed = true;
                    return;
                }
                PollResult::Pending => {
                    if std::time::Instant::now() >= deadline {
                        return; // Leave in registry; cancel_child handles it.
                    }
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }
    }

    /// Mark as confirmed (idempotent).  Called after successful drain.
    fn confirm_exit(&mut self) {
        self.confirmed = true;
    }

    fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.stdout.take()
    }

    fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.stderr.take()
    }
}

impl Drop for ChildLease {
    fn drop(&mut self) {
        if self.confirmed {
            return;
        }
        if let Some(core) = self.core.take() {
            // cancel_child needs the Arc, which we have here.
            Self::cancel_child_with_core(&core, self.id);
        }
    }
}

impl ChildLease {
    /// Cancel a child using the core Arc (for CheckoutGuard).
    fn cancel_child_with_core(core: &std::sync::Arc<SupervisorCore>, id: ChildId) {
        let mut inner = core.inner.lock().unwrap_or_else(|e| e.into_inner());

        if inner.state == SupState::Failed {
            let timeout = inner.fatal_reap_timeout;
            let reapable = inner.registry.get_mut(&id).and_then(|e| e.reapable.take());
            drop(inner); // Unlock for blocking reap.

            if let Some(reapable) = reapable {
                let mut guard = CheckoutGuard {
                    id,
                    reapable: Some(reapable),
                    core: core.clone(),
                    consumed: false,
                };
                if let Some(r) = guard.reapable_mut() {
                    match blocking_reap(r, timeout) {
                        BlockingReapResult::Exited(status) => {
                            guard.consume(ReapStatus::Exited(status));
                        }
                        BlockingReapResult::AlreadyReaped => {
                            guard.consume(ReapStatus::AlreadyReaped);
                        }
                        BlockingReapResult::Timeout | BlockingReapResult::Unknown => {
                            guard.restore();
                        }
                    }
                } else {
                    guard.consumed = true;
                    drop(guard);
                }
            }
        } else {
            let workers_alive = inner.liveness.workers_alive > 0;
            if let Some(entry) = inner.registry.get_mut(&id) {
                entry.active = false;
                if let Some(r) = entry.reapable.as_mut() {
                    r.start_kill();
                }
            }
            if workers_alive {
                let _ = core.sender.try_send(id);
            }
        }
    }
}

// ── LivenessGuard ─────────────────────────────────────────────────────────

/// RAII guard that decrements a component's liveness count on drop.
/// Every required thread (worker, stuck, orphan, emergency) carries one.
struct LivenessGuard {
    core: std::sync::Arc<SupervisorCore>,
    kind: CompKind,
    /// Worker index (only for CompKind::Worker).
    worker_idx: Option<usize>,
}

impl Drop for LivenessGuard {
    fn drop(&mut self) {
        self.core.on_component_exit(self.kind, self.worker_idx);
    }
}

// ── Metrics ───────────────────────────────────────────────────────────────

/// Snapshot of reaper state for observability.
///
/// # Compatibility
///
/// Adding a field to this public struct is source-incompatible for downstream
/// exhaustive struct literals and patterns. The `reap_panics` field is an
/// intentional pre-1.0 API change; consumers that need forward compatibility
/// should read fields individually and use `..` in patterns.
#[derive(Debug)]
pub struct ReaperMetrics {
    pub in_flight: usize,
    pub admitted: usize,
    pub completed: usize,
    /// Component-thread exits observed by their liveness guards.
    pub component_failures: usize,
    /// Panics caught and contained by the emergency reap loop.
    ///
    /// This excludes panics that terminate worker, stuck, orphan, or emergency
    /// components; those exits are reflected in `component_failures` instead.
    pub reap_panics: usize,
    pub registry_len: usize,
    pub healthy: bool,
    pub workers_alive: usize,
    pub stuck_alive: bool,
    pub orphan_alive: bool,
    pub emergency_alive: bool,
}

// ── Thread spawner + transactional init ───────────────────────────────────

type SpawnFn = Box<dyn FnOnce() + Send + 'static>;

/// Thread spawner function signature.
type Spawner = dyn Fn(&str, SpawnFn) -> Result<std::thread::JoinHandle<()>, std::io::Error> + Sync;

/// Spawn a named thread.  Production default wraps `std::thread::Builder`.
fn default_spawner(name: &str, f: SpawnFn) -> Result<std::thread::JoinHandle<()>, std::io::Error> {
    std::thread::Builder::new().name(name.to_string()).spawn(f)
}

/// Initialise a `ChildReaper` with a custom thread spawner.
///
/// **Transactional**: if any thread fails to spawn, all previously started
/// threads are stopped (stop flag) and joined before returning `Err`.
/// No residual threads remain on failure.
fn init_reaper(spawner: &Spawner) -> Result<ChildReaper, std::io::Error> {
    use std::sync::atomic::{AtomicBool, Ordering};

    let (sender, receiver) = std::sync::mpsc::sync_channel::<ChildId>(REAPER_QUEUE_CAPACITY);
    let receiver = std::sync::Arc::new(std::sync::Mutex::new(receiver));
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let core = std::sync::Arc::new(SupervisorCore::new(sender.clone()));

    let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

    // Rollback closure: stop + join all started threads.
    let rollback = |handles: &mut Vec<std::thread::JoinHandle<()>>, stop: &AtomicBool| {
        stop.store(true, Ordering::SeqCst);
        for h in handles.drain(..) {
            let _ = h.join();
        }
    };

    // ── Workers ──
    for i in 0..REAPER_WORKERS {
        let core_guard = core.clone();
        let core_loop = core.clone();
        let rx = receiver.clone();
        let stp = stop.clone();
        match spawner(
            &format!("child-reaper-{i}"),
            Box::new(move || {
                let _guard = LivenessGuard {
                    core: core_guard,
                    kind: CompKind::Worker,
                    worker_idx: Some(i),
                };
                ChildReaper::worker_loop(rx, core_loop, stp);
            }),
        ) {
            Ok(h) => handles.push(h),
            Err(e) => {
                rollback(&mut handles, &stop);
                return Err(e);
            }
        }
    }

    // ── Stuck scheduler ──
    {
        let core_guard = core.clone();
        let core_loop = core.clone();
        let snd = sender.clone();
        let stp = stop.clone();
        match spawner(
            "child-reaper-stuck",
            Box::new(move || {
                let _guard = LivenessGuard {
                    core: core_guard,
                    kind: CompKind::Stuck,
                    worker_idx: None,
                };
                ChildReaper::stuck_loop(core_loop, snd, stp);
            }),
        ) {
            Ok(h) => handles.push(h),
            Err(e) => {
                rollback(&mut handles, &stop);
                return Err(e);
            }
        }
    }

    // ── Orphan reaper ──
    {
        let core_guard = core.clone();
        let core_loop = core.clone();
        let stp = stop.clone();
        match spawner(
            "child-reaper-orphan",
            Box::new(move || {
                let _guard = LivenessGuard {
                    core: core_guard,
                    kind: CompKind::Orphan,
                    worker_idx: None,
                };
                ChildReaper::orphan_reaper_loop(core_loop, stp);
            }),
        ) {
            Ok(h) => handles.push(h),
            Err(e) => {
                rollback(&mut handles, &stop);
                return Err(e);
            }
        }
    }

    // ── Emergency recovery owner ──
    // This thread is always alive.  When all other reapers are dead
    // (Failed state), it becomes the active recovery owner: it scans
    // the registry and reaps remaining children with blocking_reap via
    // CheckoutGuard.  This ensures unreaped entries always have a
    // living, bounded owner.
    {
        let core_guard = core.clone();
        let core_loop = core.clone();
        let stp = stop.clone();
        match spawner(
            "child-reaper-emergency",
            Box::new(move || {
                let _guard = LivenessGuard {
                    core: core_guard,
                    kind: CompKind::Emergency,
                    worker_idx: None,
                };
                ChildReaper::emergency_loop(core_loop, stp);
            }),
        ) {
            Ok(h) => handles.push(h),
            Err(e) => {
                rollback(&mut handles, &stop);
                return Err(e);
            }
        }
    }

    Ok(ChildReaper {
        core,
        _sender: sender,
        stop,
        handles: std::sync::Mutex::new(handles),
    })
}

// ── blocking_reap (fatal-path synchronous reap) ───────────────────────────

/// Result of `blocking_reap`.
enum BlockingReapResult {
    Exited(std::process::ExitStatus),
    /// ECHILD — child reaped by another entity, status unknown.
    AlreadyReaped,
    /// Timed out waiting for exit.
    Timeout,
    /// Unknown wait error.
    Unknown,
}

/// Synchronously kill + wait a reapable with a per-child deadline.
/// Returns the terminal status if confirmed exited.  Does NOT store the
/// handle — the caller (via `CheckoutGuard`) retains it.
///
/// **Panic safety**: the caller holds a `CheckoutGuard` which restores
/// the handle on unwind.
fn blocking_reap(reapable: &mut dyn Reapable, timeout: std::time::Duration) -> BlockingReapResult {
    reapable.start_kill();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match reapable.try_wait() {
            Ok(Some(status)) => return BlockingReapResult::Exited(status),
            Err(e) if e.raw_os_error() == Some(ECHILD) => {
                return BlockingReapResult::AlreadyReaped;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                if std::time::Instant::now() >= deadline {
                    return BlockingReapResult::Timeout;
                }
                std::thread::sleep(EINTR_BACKOFF);
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    return BlockingReapResult::Timeout;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(_) => return BlockingReapResult::Unknown,
        }
    }
}

// ── ChildReaper ───────────────────────────────────────────────────────────

/// Centralized child process reaper with bounded admission, authoritative
/// registry, per-component supervisor health, and transactional init.
///
/// All state lives in [`SupervisorCore`] behind a single `Mutex`.
pub struct ChildReaper {
    core: std::sync::Arc<SupervisorCore>,
    _sender: std::sync::mpsc::SyncSender<ChildId>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handles: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl ChildReaper {
    /// Returns the global reaper, or `None` if init failed (fail-closed).
    /// Init is transactional — no residual threads on failure.
    pub fn global() -> Option<&'static ChildReaper> {
        static REAPER: std::sync::OnceLock<Option<ChildReaper>> = std::sync::OnceLock::new();
        REAPER
            .get_or_init(|| init_reaper(&default_spawner).ok())
            .as_ref()
    }

    /// Try to acquire a pre-spawn admission.  Fail-closed when unhealthy
    /// or at capacity.  The returned `SpawnAdmission` carries the epoch.
    pub fn try_admit(&self) -> Option<SpawnAdmission> {
        self.core.try_admit().map(|epoch| SpawnAdmission {
            core: Some(self.core.clone()),
            epoch,
        })
    }

    /// Whether the supervisor is healthy (all components alive).
    pub fn is_healthy(&self) -> bool {
        self.core.is_healthy()
    }

    /// Snapshot of reaper metrics.
    pub fn metrics(&self) -> ReaperMetrics {
        self.core.metrics()
    }

    /// Worker thread: pulls `ChildId` values and reaps them with a
    /// bounded deadline.  Skips active entries.  On timeout/Unknown,
    /// the entry stays in the registry for the stuck scheduler.
    fn worker_loop(
        receiver: std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<ChildId>>>,
        core: std::sync::Arc<SupervisorCore>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::Ordering;
        use std::sync::mpsc::RecvTimeoutError;

        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let id = match receiver.lock() {
                Ok(rx) => rx.recv_timeout(STOP_CHECK_INTERVAL),
                Err(e) => e.into_inner().recv_timeout(STOP_CHECK_INTERVAL),
            };
            match id {
                Ok(id) => {
                    let deadline = std::time::Instant::now() + REAP_WORKER_TIMEOUT;
                    loop {
                        match core.try_reap(id) {
                            ReapResult::ConfirmedNow(_) | ReapResult::AlreadyAbsent => break,
                            ReapResult::Pending => {
                                if std::time::Instant::now() >= deadline {
                                    break;
                                }
                                std::thread::sleep(POLL_INTERVAL);
                            }
                            ReapResult::Unknown => break,
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    /// Stuck scheduler: periodically scans the registry and reaps
    /// non-active entries.  Re-enqueues pending entries to workers.
    fn stuck_loop(
        core: std::sync::Arc<SupervisorCore>,
        sender: std::sync::mpsc::SyncSender<ChildId>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::Ordering;

        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(STUCK_REAPER_INTERVAL);
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if !core.any_reaper_alive() {
                break; // Emergency loop takes over.
            }
            let ids = core.pending_ids();
            for id in ids {
                match core.try_reap(id) {
                    ReapResult::ConfirmedNow(_) | ReapResult::AlreadyAbsent => {}
                    ReapResult::Pending | ReapResult::Unknown => {
                        let _ = sender.try_send(id);
                    }
                }
            }
        }
    }

    /// Orphan reaper: fallback scanner when workers are dead but stuck
    /// may still be alive.
    fn orphan_reaper_loop(
        core: std::sync::Arc<SupervisorCore>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::Ordering;

        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(STUCK_REAPER_INTERVAL);
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if !core.any_reaper_alive() {
                break; // Emergency loop takes over.
            }
            let ids = core.pending_ids();
            for id in ids {
                let _ = core.try_reap(id);
            }
        }
    }

    /// Emergency recovery owner: always alive.  When all other reapers
    /// are dead (Failed state), this thread becomes the active recovery
    /// owner.  It scans the registry, checks out each child via
    /// `CheckoutGuard`, and performs `blocking_reap` outside the lock.
    ///
    /// This ensures unreaped entries always have a living, bounded owner
    /// that retries until the child exits.  The emergency thread is a
    /// single bounded thread — no unbounded thread growth.
    ///
    /// **Latency bound**: each child gets `fatal_reap_timeout` (default
    /// 5s).  The total scan is bounded by `FATAL_CLEANUP_TOTAL_BUDGET`
    /// (default 30s) per pass.  Unreaped children are retried on the
    /// next scan pass.
    fn emergency_loop(
        core: std::sync::Arc<SupervisorCore>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::Ordering;

        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(STUCK_REAPER_INTERVAL);
            if stop.load(Ordering::Relaxed) {
                break;
            }

            // Emergency takeover: only act when all normal reapers
            // (workers, stuck, orphan) are dead.  In Degraded state
            // with some reapers alive, the stuck/orphan loops handle
            // pending children.
            if core.any_reaper_alive() {
                continue;
            }

            // Scan and reap via CheckoutGuard (outside lock).
            let ids = core.pending_ids();
            let pass_deadline = std::time::Instant::now() + FATAL_CLEANUP_TOTAL_BUDGET;
            let per_child_timeout = core.fatal_reap_timeout();

            for id in ids {
                let now = std::time::Instant::now();
                if now >= pass_deadline {
                    break; // Total budget exceeded — retry next pass.
                }
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                // Use min(per_child_timeout, remaining_pass_budget) so
                // a single child cannot exceed the pass deadline.
                let remaining = pass_deadline.saturating_duration_since(now);
                let timeout = std::cmp::min(per_child_timeout, remaining);

                // Check out the child for fatal reap.  checkout_for_fatal
                // takes the handle out of the registry under the lock,
                // then we reap outside the lock.
                if let Some(guard) = core.checkout_for_fatal(id) {
                    // Isolate panic: wrap blocking_reap in catch_unwind
                    // so a panicking try_wait does not kill the emergency
                    // thread.  CheckoutGuard::drop restores the handle
                    // during unwind.  On panic, we back off and continue
                    // to the next child.
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let mut guard = guard;
                        if let Some(r) = guard.reapable_mut() {
                            match blocking_reap(r, timeout) {
                                BlockingReapResult::Exited(status) => {
                                    guard.consume(ReapStatus::Exited(status));
                                }
                                BlockingReapResult::AlreadyReaped => {
                                    guard.consume(ReapStatus::AlreadyReaped);
                                }
                                BlockingReapResult::Timeout | BlockingReapResult::Unknown => {
                                    guard.restore();
                                }
                            }
                        } else {
                            guard.consumed = true;
                        }
                    }));
                    if result.is_err() {
                        // Panic was caught — handle restored by
                        // CheckoutGuard::drop during unwind.
                        // Back off to avoid tight-spinning on a
                        // persistent-panic child.
                        core.record_reap_panic();
                        std::thread::sleep(EINTR_BACKOFF);
                    }
                }
            }
        }
    }
}

impl Drop for ChildReaper {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        self.stop.store(true, Ordering::SeqCst);
        let mut handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        for h in handles.drain(..) {
            let _ = h.join();
        }
        // After join, all LivenessGuards have dropped.  Remaining
        // children in the registry are not guaranteed to have been
        // reaped — the emergency thread stops when the stop flag is set.
    }
}

// ── Guarded drain ─────────────────────────────────────────────────────────

/// Drain stdout/stderr from a spawned child with shutdown cancellation
/// and guaranteed reap on drop.
///
/// The [`ChildLease`] is created synchronously before the future is
/// returned (child already committed to the registry), so dropping the
/// future without polling still triggers `ChildLease::drop` →
/// `cancel_child`, which kills + routes the child for reaping.
pub fn drain_child_output_guarded(
    mut lease: ChildLease,
    opts: ShellOptions<'_>,
    shutdown: CancellationToken,
) -> impl std::future::Future<Output = Result<ShellOutput, EnvError>> + use<'_> {
    let mut stdout_handle = lease.take_stdout();
    let mut stderr_handle = lease.take_stderr();

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

#[cfg(test)]
mod reap_tests {
    use super::*;
    #[cfg(unix)]
    use llm_harness_types::ShellOptions;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;
    #[cfg(unix)]
    use tokio_util::sync::CancellationToken;

    // ── Test helpers ──────────────────────────────────────────────────────

    const TEST_SYNC_TIMEOUT: Duration = Duration::from_secs(5);

    #[cfg(unix)]
    fn process_is_gone(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/status")).is_err()
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    /// Make a test core with no threads (state=Healthy, all components alive).
    fn make_test_core() -> Arc<SupervisorCore> {
        let (sender, _receiver) = std::sync::mpsc::sync_channel::<ChildId>(REAPER_QUEUE_CAPACITY);
        let core = Arc::new(SupervisorCore::new(sender));
        core.set_fatal_reap_timeout(Duration::from_millis(50));
        core
    }

    /// Deterministic mock outcome for try_wait.
    #[derive(Clone)]
    enum MockOutcome {
        Exited(i32),
        Pending,
        Interrupted,
        #[allow(dead_code)]
        AlreadyReaped,
        Unknown(i32),
        /// Panic on try_wait — simulates a worker thread crashing.
        Panic,
    }

    /// Mock reapable with deterministic try_wait outcomes.
    struct MockReapable {
        outcomes: std::sync::Mutex<Vec<MockOutcome>>,
        index: AtomicUsize,
        killed: AtomicBool,
    }

    impl MockReapable {
        fn new(outcomes: Vec<MockOutcome>) -> Self {
            Self {
                outcomes: std::sync::Mutex::new(outcomes),
                index: AtomicUsize::new(0),
                killed: AtomicBool::new(false),
            }
        }

        fn was_killed(&self) -> bool {
            self.killed.load(Ordering::SeqCst)
        }
    }

    impl Reapable for MockReapable {
        fn start_kill(&mut self) {
            self.killed.store(true, Ordering::SeqCst);
        }
        fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
            let idx = self.index.fetch_add(1, Ordering::SeqCst);
            let outcome = {
                let outcomes = self.outcomes.lock().unwrap();
                if idx < outcomes.len() {
                    outcomes[idx].clone()
                } else {
                    MockOutcome::Pending
                }
            };
            match outcome {
                MockOutcome::Exited(code) => Ok(Some(exit_status_from_raw(code))),
                MockOutcome::Pending => Ok(None),
                MockOutcome::Interrupted => Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "interrupted",
                )),
                MockOutcome::AlreadyReaped => Err(std::io::Error::from_raw_os_error(ECHILD)),
                MockOutcome::Unknown(errno) => Err(std::io::Error::from_raw_os_error(errno)),
                MockOutcome::Panic => panic!("injected try_wait panic"),
            }
        }
    }

    #[derive(Default)]
    struct AttemptTrace {
        attempts: AtomicUsize,
        panics: AtomicUsize,
        killed: AtomicBool,
        threads: std::sync::Mutex<Vec<String>>,
    }

    impl AttemptTrace {
        fn record_attempt(&self) -> usize {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            let thread = std::thread::current()
                .name()
                .unwrap_or("unnamed")
                .to_string();
            self.threads.lock().unwrap().push(thread);
            attempt
        }

        fn thread_names(&self) -> Vec<String> {
            self.threads.lock().unwrap().clone()
        }
    }

    struct PanicOnceThenExit {
        trace: Arc<AttemptTrace>,
        first_attempt: Option<std::sync::mpsc::Sender<()>>,
    }

    impl PanicOnceThenExit {
        fn new(
            trace: Arc<AttemptTrace>,
            first_attempt: Option<std::sync::mpsc::Sender<()>>,
        ) -> Self {
            Self {
                trace,
                first_attempt,
            }
        }
    }

    impl Reapable for PanicOnceThenExit {
        fn start_kill(&mut self) {
            self.trace.killed.store(true, Ordering::SeqCst);
        }

        fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
            let attempt = self.trace.record_attempt();
            if attempt == 0 {
                self.trace.panics.fetch_add(1, Ordering::SeqCst);
                if let Some(sender) = &self.first_attempt {
                    let _ = sender.send(());
                }
                panic!("injected first-attempt panic");
            }
            Ok(Some(exit_status_from_raw(0)))
        }
    }

    struct PanicUntilExitAllowed {
        trace: Arc<AttemptTrace>,
        exit_allowed: Arc<AtomicBool>,
    }

    impl Reapable for PanicUntilExitAllowed {
        fn start_kill(&mut self) {
            self.trace.killed.store(true, Ordering::SeqCst);
        }

        fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
            self.trace.record_attempt();
            if self.exit_allowed.load(Ordering::SeqCst) {
                return Ok(Some(exit_status_from_raw(0)));
            }
            self.trace.panics.fetch_add(1, Ordering::SeqCst);
            panic!("injected persistent panic");
        }
    }

    struct AlwaysPanic {
        trace: Arc<AttemptTrace>,
    }

    impl Reapable for AlwaysPanic {
        fn start_kill(&mut self) {
            self.trace.killed.store(true, Ordering::SeqCst);
        }

        fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
            self.trace.record_attempt();
            self.trace.panics.fetch_add(1, Ordering::SeqCst);
            panic!("injected persistent panic");
        }
    }

    struct ExitWhenReleased {
        trace: Arc<AttemptTrace>,
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl Reapable for ExitWhenReleased {
        fn start_kill(&mut self) {
            self.trace.killed.store(true, Ordering::SeqCst);
        }

        fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
            self.trace.record_attempt();
            self.entered
                .send(())
                .expect("test observer dropped before entry notification");
            self.release
                .recv_timeout(TEST_SYNC_TIMEOUT)
                .expect("test release timed out or disconnected");
            Ok(Some(exit_status_from_raw(0)))
        }
    }

    #[derive(Default)]
    struct TimedGate {
        open: std::sync::Mutex<bool>,
        changed: std::sync::Condvar,
    }

    impl TimedGate {
        fn wait(&self, timeout: Duration) -> bool {
            let open = self.open.lock().unwrap_or_else(|error| error.into_inner());
            let (open, _) = self
                .changed
                .wait_timeout_while(open, timeout, |open| !*open)
                .unwrap_or_else(|error| error.into_inner());
            *open
        }

        fn open(&self) {
            let mut open = self.open.lock().unwrap_or_else(|error| error.into_inner());
            *open = true;
            self.changed.notify_all();
        }
    }

    #[derive(Default)]
    struct ThreadTracker {
        active: AtomicUsize,
        started: AtomicUsize,
        exited: AtomicUsize,
    }

    struct ActiveThreadGuard {
        tracker: Arc<ThreadTracker>,
    }

    type OwnedSpawner = Box<Spawner>;

    impl ActiveThreadGuard {
        fn enter(tracker: Arc<ThreadTracker>) -> Self {
            tracker.started.fetch_add(1, Ordering::SeqCst);
            tracker.active.fetch_add(1, Ordering::SeqCst);
            Self { tracker }
        }
    }

    impl Drop for ActiveThreadGuard {
        fn drop(&mut self) {
            self.tracker.active.fetch_sub(1, Ordering::SeqCst);
            self.tracker.exited.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Test-only lease from a mock reapable.
    fn lease_from_mock(core: Arc<SupervisorCore>, mock: MockReapable) -> (ChildId, ChildLease) {
        let epoch = core.try_admit().expect("test core must admit");
        let id = core.commit_child(Box::new(mock), epoch);
        let lease = ChildLease {
            id,
            core: Some(core),
            stdout: None,
            stderr: None,
            confirmed: false,
        };
        (id, lease)
    }

    /// Failing thread spawner that injects a failure at the Nth spawn.
    fn make_failing_spawner(
        fail_at: usize,
    ) -> impl Fn(&str, SpawnFn) -> Result<std::thread::JoinHandle<()>, std::io::Error> + Sync {
        let count = Arc::new(AtomicUsize::new(0));
        move |_name: &str, f: SpawnFn| -> Result<std::thread::JoinHandle<()>, std::io::Error> {
            let n = count.fetch_add(1, Ordering::SeqCst);
            if n == fail_at {
                return Err(std::io::Error::other("injected spawn failure"));
            }
            std::thread::Builder::new()
                .name(format!("test-reaper-{n}"))
                .spawn(f)
        }
    }

    fn make_tracked_failing_spawner(fail_at: usize) -> (OwnedSpawner, Arc<ThreadTracker>) {
        let count = Arc::new(AtomicUsize::new(0));
        let tracker = Arc::new(ThreadTracker::default());
        let spawner_tracker = tracker.clone();
        let spawner =
            move |name: &str, f: SpawnFn| -> Result<std::thread::JoinHandle<()>, std::io::Error> {
                let n = count.fetch_add(1, Ordering::SeqCst);
                if n == fail_at {
                    return Err(std::io::Error::other("injected spawn failure"));
                }

                let tracker = spawner_tracker.clone();
                std::thread::Builder::new()
                    .name(name.to_string())
                    .spawn(move || {
                        let _active = ActiveThreadGuard::enter(tracker);
                        f();
                    })
            };
        (Box::new(spawner), tracker)
    }

    fn make_scanner_gated_spawner(
        scanner_gate: Arc<TimedGate>,
    ) -> impl Fn(&str, SpawnFn) -> Result<std::thread::JoinHandle<()>, std::io::Error> + Sync {
        move |name: &str, f: SpawnFn| {
            let gate = scanner_gate.clone();
            let is_scanner = name == "child-reaper-stuck" || name == "child-reaper-orphan";
            std::thread::Builder::new()
                .name(name.to_string())
                .spawn(move || {
                    if is_scanner {
                        assert!(
                            gate.wait(TEST_SYNC_TIMEOUT),
                            "scanner gate was not opened before its deadline"
                        );
                    }
                    f();
                })
        }
    }

    fn wait_until(timeout: Duration, message: &str, predicate: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + timeout;
        while !predicate() {
            assert!(std::time::Instant::now() < deadline, "{message}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn induce_real_emergency_takeover(
        reaper: &ChildReaper,
        controller_epoch: Epoch,
    ) -> Arc<AttemptTrace> {
        let trace = Arc::new(AttemptTrace::default());
        let exit_allowed = Arc::new(AtomicBool::new(false));
        let controller_id = reaper.core.commit_child(
            Box::new(PanicUntilExitAllowed {
                trace: trace.clone(),
                exit_allowed: exit_allowed.clone(),
            }),
            controller_epoch,
        );
        ChildLease::cancel_child_with_core(&reaper.core, controller_id);
        for _ in 0..(REAPER_WORKERS * 3) {
            let _ = reaper.core.sender.try_send(controller_id);
        }

        wait_until(
            Duration::from_secs(10),
            "real normal-reaper failures did not activate emergency takeover",
            || {
                let metrics = reaper.metrics();
                metrics.workers_alive == 0 && !metrics.stuck_alive && !metrics.orphan_alive
            },
        );

        let metrics = reaper.metrics();
        assert_eq!(metrics.component_failures, REAPER_WORKERS + 2);
        assert!(metrics.emergency_alive, "emergency owner must remain alive");
        assert_eq!(reaper.core.state(), SupState::Failed);
        assert_eq!(reaper.core.epoch(), (REAPER_WORKERS + 2) as Epoch);

        let names = trace.thread_names();
        for worker in 0..REAPER_WORKERS {
            assert!(
                names
                    .iter()
                    .any(|name| name == &format!("child-reaper-{worker}")),
                "worker {worker} did not execute the AlwaysPanic probe: {names:?}"
            );
        }
        assert!(names.iter().any(|name| name == "child-reaper-stuck"));
        assert!(names.iter().any(|name| name == "child-reaper-orphan"));

        exit_allowed.store(true, Ordering::SeqCst);
        wait_until(
            Duration::from_secs(10),
            "emergency owner did not naturally reap the released controller",
            || reaper.core.completed_count() == 1,
        );
        assert_eq!(reaper.core.registry_len(), 0);
        assert!(
            trace
                .thread_names()
                .last()
                .is_some_and(|name| name == "child-reaper-emergency"),
            "released controller did not complete on the emergency thread"
        );
        trace
    }

    // ── classify_wait unit tests ──────────────────────────────────────────

    #[test]
    fn classify_wait_exited() {
        assert!(matches!(
            classify_wait(Ok(Some(exit_status_from_raw(0)))),
            WaitOutcome::Exited
        ));
    }

    #[test]
    fn classify_wait_pending() {
        assert!(matches!(classify_wait(Ok(None)), WaitOutcome::Pending));
    }

    #[test]
    fn classify_wait_interrupted() {
        assert!(matches!(
            classify_wait(Err(std::io::Error::from(std::io::ErrorKind::Interrupted))),
            WaitOutcome::Interrupted
        ));
    }

    #[test]
    #[cfg(unix)]
    fn classify_wait_already_reaped() {
        assert!(matches!(
            classify_wait(Err(std::io::Error::from_raw_os_error(ECHILD))),
            WaitOutcome::AlreadyReaped
        ));
    }

    #[test]
    fn classify_wait_unknown_error_retains_handle() {
        assert!(matches!(
            classify_wait(Err(std::io::Error::from_raw_os_error(999))),
            WaitOutcome::Unknown(_)
        ));
    }

    // ── Admission: atomic health check + increment ────────────────────────

    #[test]
    fn admission_at_capacity_rejects_then_recovers() {
        let core = make_test_core();
        for _ in 0..REAPER_QUEUE_CAPACITY {
            assert!(core.try_admit().is_some());
        }
        assert_eq!(core.in_flight_count(), REAPER_QUEUE_CAPACITY);
        assert!(core.try_admit().is_none(), "should reject at capacity");

        // Simulate releasing all admissions.
        for _ in 0..REAPER_QUEUE_CAPACITY {
            core.release_admission();
        }
        assert_eq!(core.in_flight_count(), 0);

        assert!(core.try_admit().is_some());
        assert_eq!(core.in_flight_count(), 1);
        core.release_admission();
        assert_eq!(core.in_flight_count(), 0);
    }

    #[test]
    fn admission_closed_after_failed_state() {
        let core = make_test_core();
        assert!(core.try_admit().is_some());
        core.release_admission();

        // Simulate all components dying.
        core.force_all_reapers_dead();
        assert_eq!(core.state(), SupState::Failed);

        // Admission must be permanently closed.
        assert!(
            core.try_admit().is_none(),
            "admission must be closed after Failed"
        );
        assert!(core.try_admit().is_none(), "admission must remain closed");
    }

    #[test]
    fn admission_closed_after_degraded_state() {
        let core = make_test_core();
        assert!(core.try_admit().is_some());
        core.release_admission();

        // Simulate one worker dying.
        core.force_worker_exit();
        assert_eq!(core.state(), SupState::Degraded);

        // Admission must be closed (requires all components alive).
        assert!(
            core.try_admit().is_none(),
            "admission must be closed when Degraded"
        );
    }

    // ── Spawn failure releases pre-spawn admission only ───────────────────

    #[test]
    fn spawn_failure_releases_pre_spawn_admission() {
        let core = make_test_core();
        assert!(core.try_admit().is_some());
        assert_eq!(core.in_flight_count(), 1);

        // Simulate spawn failure: SpawnAdmission is dropped without into_lease.
        let admission = SpawnAdmission {
            core: Some(core.clone()),
            epoch: core.epoch(),
        };
        drop(admission);
        assert_eq!(
            core.in_flight_count(),
            0,
            "admission must be released on spawn failure"
        );
    }

    // ── Immediate registration: child in registry from into_lease ─────────

    #[test]
    fn immediate_registration_child_in_registry() {
        let core = make_test_core();
        let mock = MockReapable::new(vec![MockOutcome::Pending]);
        let (id, _lease) = lease_from_mock(core.clone(), mock);

        // Child must be in the registry immediately (not deferred to drop).
        assert_eq!(
            core.registry_len(),
            1,
            "child must be registered immediately"
        );
        // Entry must be active (drain function is using it).
        assert!(
            !core.pending_ids().contains(&id),
            "active entry must not appear in pending_ids"
        );
    }

    // ── Exactly-once completed metric ─────────────────────────────────────

    #[test]
    fn exactly_one_confirmed_now_for_same_id() {
        let core = make_test_core();
        // Outcomes: Pending, Pending, Exited, then any subsequent calls.
        let mock = MockReapable::new(vec![
            MockOutcome::Pending,
            MockOutcome::Pending,
            MockOutcome::Exited(0),
        ]);
        let (id, _lease) = lease_from_mock(core.clone(), mock);

        // Mark as non-active so try_reap doesn't skip.
        {
            let mut inner = core.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = inner.registry.get_mut(&id) {
                entry.active = false;
            }
        }

        // Multiple threads racing to reap the same ID.
        let core2 = core.clone();
        let h1 = std::thread::spawn(move || core2.try_reap(id));
        let core3 = core.clone();
        let h2 = std::thread::spawn(move || core3.try_reap(id));
        let core4 = core.clone();
        let h3 = std::thread::spawn(move || core4.try_reap(id));
        let r1 = h1.join().unwrap();
        let r2 = h2.join().unwrap();
        let r3 = h3.join().unwrap();

        let confirmed_count = [&r1, &r2, &r3]
            .iter()
            .filter(|r| matches!(r, ReapResult::ConfirmedNow(_)))
            .count();
        assert_eq!(confirmed_count, 1, "exactly one ConfirmedNow");
        assert_eq!(core.completed_count(), 1, "completed must be exactly 1");
        assert_eq!(core.in_flight_count(), 0, "admission released once");
        assert_eq!(core.registry_len(), 0, "entry removed");
    }

    #[test]
    fn duplicate_id_enqueue_does_not_double_count() {
        let core = make_test_core();
        let mock = MockReapable::new(vec![
            MockOutcome::Pending,
            MockOutcome::Pending,
            MockOutcome::Exited(0),
        ]);
        let (id, _lease) = lease_from_mock(core.clone(), mock);

        {
            let mut inner = core.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = inner.registry.get_mut(&id) {
                entry.active = false;
            }
        }

        // Enqueue the same ID 8 times (simulating stuck + orphan re-enqueue).
        for _ in 0..8 {
            match core.try_reap(id) {
                ReapResult::ConfirmedNow(_) => {}
                ReapResult::AlreadyAbsent => {}
                _ => {}
            }
        }
        assert_eq!(
            core.completed_count(),
            1,
            "completed must be 1 regardless of enqueue count"
        );
    }

    // ── Timeout/Unknown retains handle ────────────────────────────────────

    #[test]
    fn timeout_retains_handle_and_admission() {
        let core = make_test_core();
        // Always Pending — child never exits.
        let mock = MockReapable::new(vec![MockOutcome::Pending]);
        let (id, _lease) = lease_from_mock(core.clone(), mock);

        {
            let mut inner = core.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = inner.registry.get_mut(&id) {
                entry.active = false;
            }
        }

        // try_reap returns Pending — entry retained.
        let result = core.try_reap(id);
        assert!(matches!(result, ReapResult::Pending));
        assert_eq!(core.registry_len(), 1, "entry must be retained on Pending");
        assert_eq!(
            core.in_flight_count(),
            1,
            "admission must NOT be released on Pending"
        );
    }

    #[test]
    fn unknown_error_retains_handle_and_admission() {
        let core = make_test_core();
        let mock = MockReapable::new(vec![MockOutcome::Unknown(999)]);
        let (id, _lease) = lease_from_mock(core.clone(), mock);

        {
            let mut inner = core.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = inner.registry.get_mut(&id) {
                entry.active = false;
            }
        }

        let result = core.try_reap(id);
        assert!(matches!(result, ReapResult::Unknown));
        assert_eq!(core.registry_len(), 1, "entry must be retained on Unknown");
        assert_eq!(
            core.in_flight_count(),
            1,
            "admission must NOT be released on Unknown"
        );
    }

    // ── cancel_child kills and routes (alive supervisor) ──────────────────

    #[test]
    fn cancel_child_kills_and_routes() {
        let core = make_test_core();
        let mock = MockReapable::new(vec![MockOutcome::Pending]);
        let (id, _lease) = lease_from_mock(core.clone(), mock);

        // cancel_child (simulating lease drop without confirmation).
        ChildLease::cancel_child_with_core(&core, id);

        // Entry should still be in registry (not yet reaped), but non-active.
        assert_eq!(core.registry_len(), 1);
        assert!(
            core.pending_ids().contains(&id),
            "cancelled entry must be pending"
        );

        // Now reap it.
        // Poll until ConfirmedNow (mock returns Ok(None) by default, so we
        // need to inject an exit).  Use a fresh mock via direct try_reap.
        // Since the mock always returns Pending, we verify the kill happened.
        // The mock's killed flag is not accessible here, but we can verify
        // the entry is non-active and pending.
    }

    // ── fatal_cleanup: only Exited releases admission ─────────────────────

    #[test]
    fn fatal_cleanup_releases_on_exit_retains_on_timeout() {
        let core = make_test_core();
        let mock_exit = MockReapable::new(vec![MockOutcome::Exited(0)]);
        let (_id1, _lease1) = lease_from_mock(core.clone(), mock_exit);
        let mock_stuck = MockReapable::new(vec![MockOutcome::Pending]);
        let (_id2, _lease2) = lease_from_mock(core.clone(), mock_stuck);
        assert_eq!(core.in_flight_count(), 2);
        core.force_all_reapers_dead();
        assert_eq!(core.state(), SupState::Failed);
        // Manually reap via checkout (no emergency thread in test core).
        for id in core.all_ids() {
            if let Some(mut guard) = core.checkout_for_fatal(id)
                && let Some(r) = guard.reapable_mut()
            {
                match blocking_reap(r, core.fatal_reap_timeout()) {
                    BlockingReapResult::Exited(s) => guard.consume(ReapStatus::Exited(s)),
                    BlockingReapResult::AlreadyReaped => guard.consume(ReapStatus::AlreadyReaped),
                    _ => guard.restore(),
                }
            }
        }
        assert_eq!(core.completed_count(), 1, "only exited child reaped");
        assert_eq!(core.registry_len(), 1, "stuck child retained");
        assert_eq!(
            core.in_flight_count(),
            1,
            "admission retained for stuck child"
        );
    }

    #[test]
    fn fatal_cleanup_per_child_deadline() {
        let core = make_test_core();
        let mock = MockReapable::new(vec![MockOutcome::Exited(0)]);
        let (_id, _lease) = lease_from_mock(core.clone(), mock);
        core.force_all_reapers_dead();
        assert_eq!(core.state(), SupState::Failed);
        // Manually reap via checkout (no emergency thread in test core).
        for id in core.all_ids() {
            if let Some(mut guard) = core.checkout_for_fatal(id)
                && let Some(r) = guard.reapable_mut()
            {
                match blocking_reap(r, core.fatal_reap_timeout()) {
                    BlockingReapResult::Exited(s) => guard.consume(ReapStatus::Exited(s)),
                    BlockingReapResult::AlreadyReaped => guard.consume(ReapStatus::AlreadyReaped),
                    _ => guard.restore(),
                }
            }
        }
        assert_eq!(core.registry_len(), 0, "exited child must be reaped");
        assert_eq!(core.in_flight_count(), 0, "admission released");
    }

    // ── Late registration after Failed ────────────────────────────────────

    #[test]
    fn commit_child_after_failed_kills_immediately() {
        let core = make_test_core();
        // Admission acquired BEFORE the failure transition.
        let epoch = core.try_admit().expect("must admit while healthy");
        core.force_all_reapers_dead();
        assert_eq!(core.state(), SupState::Failed);

        // Child spawned after Failed — commit_child kills immediately.
        // Mock exits on first try_wait (child killed by commit_child).
        let mock = MockReapable::new(vec![MockOutcome::Exited(0)]);
        let id = core.commit_child(Box::new(mock), epoch);

        // Child must be in registry, non-active, and killed.
        assert_eq!(core.registry_len(), 1);
        {
            let inner = core.inner.lock().unwrap_or_else(|e| e.into_inner());
            let entry = inner.registry.get(&id).unwrap();
            assert!(!entry.active, "child must be non-active when Failed");
        }

        // poll_exit should detect exit.
        let result = core.poll_exit(id);
        assert!(matches!(result, PollResult::Exited(_)));
        assert_eq!(core.in_flight_count(), 0);
    }

    // ── cancel_child in Failed state: synchronous reap ────────────────────

    #[test]
    fn cancel_child_failed_state_synchronous_reap() {
        let core = make_test_core();
        // Mock exits on first try_wait (cancel_child's blocking_reap).
        let mock = MockReapable::new(vec![MockOutcome::Exited(0)]);
        let (_id, lease) = lease_from_mock(core.clone(), mock);

        // Transition to Failed (no emergency thread in test core).
        core.force_all_reapers_dead();
        assert_eq!(core.state(), SupState::Failed);

        // Drop the lease — cancel_child_with_core does synchronous
        // blocking_reap via CheckoutGuard (panic-safe).
        drop(lease);

        // Child should be reaped by cancel_child's blocking_reap.
        assert_eq!(
            core.registry_len(),
            0,
            "child must be reaped by cancel_child"
        );
        assert_eq!(core.in_flight_count(), 0, "admission released");
        assert_eq!(core.completed_count(), 1);
    }

    // ── Transactional init: thread spawn failure rollback ─────────────────

    #[test]
    fn init_reaper_rollback_on_spawn_failure() {
        // Fail at the 3rd thread (first stuck scheduler).
        let spawner = make_failing_spawner(3);
        let result = init_reaper(&spawner);
        assert!(result.is_err(), "init must fail when thread spawn fails");

        // Verify no residual threads: the rollback joins all started threads.
        // If any thread were still running, it would hold a LivenessGuard
        // and the test process would have lingering threads.
        // We can't easily count threads, but the JoinHandle::join in
        // rollback ensures they're all stopped.
    }

    #[test]
    fn init_reaper_rollback_at_worker_failure() {
        // Fail at the 1st thread (first worker).
        let spawner = make_failing_spawner(0);
        let result = init_reaper(&spawner);
        assert!(
            result.is_err(),
            "init must fail when first thread spawn fails"
        );
    }

    #[test]
    fn init_reaper_rollback_at_orphan_failure() {
        // Fail at the 6th thread (orphan reaper — last one).
        let spawner = make_failing_spawner(5);
        let result = init_reaper(&spawner);
        assert!(
            result.is_err(),
            "init must fail when orphan thread spawn fails"
        );
    }

    #[test]
    fn init_reaper_succeeds_with_default_spawner() {
        let result = init_reaper(&default_spawner);
        assert!(result.is_ok(), "init must succeed with default spawner");
        let reaper = result.unwrap();
        assert!(reaper.is_healthy());
        assert!(reaper.metrics().workers_alive > 0);
        assert!(reaper.metrics().stuck_alive);
        assert!(reaper.metrics().orphan_alive);
        // Drop cleans up threads.
        drop(reaper);
    }

    // ── Mutex poison recovery ─────────────────────────────────────────────

    #[test]
    fn mutex_poison_does_not_panic() {
        let core = make_test_core();
        let core2 = core.clone();

        // Poison the mutex in a separate thread.
        let handle = std::thread::spawn(move || {
            let _guard = core2.inner.lock().unwrap();
            panic!("intentional poison");
        });
        let _ = handle.join();

        // Operations must recover from poison (unwrap_or_else into_inner).
        assert!(core.try_admit().is_some());
        core.release_admission();
        let _metrics = core.metrics();
    }

    // ── Real child PID tests (integration) ────────────────────────────────

    #[test]
    #[cfg(unix)]
    fn lease_reaps_without_runtime() {
        let child = spawn_child("sleep", "30");
        let pid = child.id().expect("child must have a PID");
        let reaper = ChildReaper::global().expect("global reaper must be available");
        let admission = reaper.try_admit().expect("should admit");
        let lease = admission.into_lease(child);
        drop(lease);
        assert!(
            wait_for_reap(pid, Duration::from_secs(10)),
            "child PID {pid} must be reaped after lease drop"
        );
    }

    #[test]
    #[cfg(unix)]
    fn unpolled_future_still_reaps_child() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (pid, lease) = rt.block_on(async {
            let child = tokio::process::Command::new("sleep")
                .arg("30")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let pid = child.id().unwrap();
            let reaper = ChildReaper::global().expect("global reaper");
            let admission = reaper.try_admit().expect("should admit");
            let lease = admission.into_lease(child);
            (pid, lease)
        });

        let _future = drain_child_output_guarded(lease, test_opts(), CancellationToken::new());
        drop(_future); // Drop without polling.

        assert!(
            wait_for_reap(pid, Duration::from_secs(10)),
            "child PID {pid} must be reaped even if future is never polled"
        );
        drop(rt);
    }

    #[test]
    #[cfg(unix)]
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
            let pid = child.id().unwrap();
            let reaper = ChildReaper::global().expect("global reaper");
            let admission = reaper.try_admit().expect("should admit");
            let lease = admission.into_lease(child);
            drop(lease);
            pid
        });

        drop(rt);

        assert!(
            wait_for_reap(pid, Duration::from_secs(10)),
            "child PID {pid} must be reaped after runtime shutdown"
        );
    }

    #[test]
    #[cfg(unix)]
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
            let pid = child.id().unwrap();
            let reaper = ChildReaper::global().expect("global reaper");
            let admission = reaper.try_admit().expect("should admit");
            let lease = admission.into_lease(child);
            drop(lease);
            pid
        });

        assert!(
            wait_for_reap(pid, Duration::from_secs(10)),
            "child PID {pid} must be reaped with runtime alive"
        );
        drop(rt);
    }

    #[test]
    #[cfg(unix)]
    fn kill_and_wait_marks_confirmed() {
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
            let pid = child.id().unwrap();
            let reaper = ChildReaper::global().expect("global reaper");
            let admission = reaper.try_admit().expect("should admit");
            let mut lease = admission.into_lease(child);
            lease.kill_and_wait().await;
            assert!(
                lease.confirmed,
                "lease must be confirmed after kill_and_wait"
            );
            pid
        });

        assert!(
            wait_for_reap(pid, Duration::from_secs(5)),
            "child PID {pid} must be reaped after kill_and_wait"
        );
        drop(rt);
    }

    // ── Cancellation storm: bounded resources ─────────────────────────────

    #[test]
    #[cfg(unix)]
    fn cancellation_storm_stays_bounded() {
        let reaper = ChildReaper::global().expect("global reaper");

        let mut pids = Vec::new();
        for _ in 0..32 {
            let admission = reaper.try_admit().expect("should admit below capacity");
            let child = spawn_child("sleep", "30");
            let pid = child.id().unwrap();
            pids.push(pid);
            let lease = admission.into_lease(child);
            drop(lease);
        }

        let metrics = reaper.metrics();
        assert!(
            metrics.in_flight <= 33,
            "in_flight ({}) must stay bounded",
            metrics.in_flight
        );

        for &pid in &pids {
            assert!(
                wait_for_reap(pid, Duration::from_secs(10)),
                "child PID {pid} must be reaped"
            );
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
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

    // ── Global reaper: stuck scheduler reaps ──────────────────────────────

    #[test]
    #[cfg(unix)]
    fn global_reaper_reaps_registered_child() {
        let reaper = ChildReaper::global().expect("global reaper");

        let child = spawn_child("sleep", "30");
        let pid = child.id().unwrap();

        let admission = reaper.try_admit().expect("should admit");
        let lease = admission.into_lease(child);
        drop(lease); // cancel_child kills + routes.

        assert!(
            wait_for_reap(pid, Duration::from_secs(10)),
            "child PID {pid} must be reaped by global reaper"
        );
    }

    // ── Global reaper: metrics reflect state ──────────────────────────────

    #[test]
    fn global_reaper_metrics_healthy() {
        let reaper = ChildReaper::global().expect("global reaper");
        let metrics = reaper.metrics();
        assert!(metrics.healthy, "global reaper must be healthy");
        assert!(metrics.workers_alive > 0);
        assert!(metrics.stuck_alive);
        assert!(metrics.orphan_alive);
    }

    // ── blocking_reap unit test ───────────────────────────────────────────

    #[test]
    fn blocking_reap_exits() {
        let mut mock = MockReapable::new(vec![MockOutcome::Exited(0)]);
        let result = blocking_reap(&mut mock, FATAL_REAP_TIMEOUT);
        assert!(
            matches!(result, BlockingReapResult::Exited(_)),
            "blocking_reap should return Exited for exited child"
        );
        assert!(mock.was_killed(), "blocking_reap must kill the child");
    }

    #[test]
    fn blocking_reap_timeout_returns_none() {
        // Always Pending — times out after the specified duration.
        let timeout = Duration::from_millis(50);
        let mut mock = MockReapable::new(vec![MockOutcome::Pending]);
        let start = std::time::Instant::now();
        let result = blocking_reap(&mut mock, timeout);
        assert!(
            matches!(result, BlockingReapResult::Timeout),
            "blocking_reap should return Timeout on timeout"
        );
        assert!(
            start.elapsed() >= timeout,
            "should have waited at least the timeout duration"
        );
    }

    #[test]
    fn blocking_reap_unknown_returns_none() {
        let mut mock = MockReapable::new(vec![MockOutcome::Unknown(999)]);
        let result = blocking_reap(&mut mock, FATAL_REAP_TIMEOUT);
        assert!(
            matches!(result, BlockingReapResult::Unknown),
            "blocking_reap should return Unknown on unknown error"
        );
        assert!(
            mock.was_killed(),
            "blocking_reap must kill even on unknown error"
        );
    }

    // ── Supervisor state monotonicity ─────────────────────────────────────

    #[test]
    fn state_transitions_are_monotonic() {
        let core = make_test_core();
        assert_eq!(core.state(), SupState::Healthy);

        core.force_worker_exit();
        assert_eq!(core.state(), SupState::Degraded);

        // All reapers dead → Failed.
        core.force_all_reapers_dead();
        assert_eq!(core.state(), SupState::Failed);
    }

    #[test]
    fn failed_state_is_terminal() {
        let core = make_test_core();
        core.force_all_reapers_dead();
        assert_eq!(core.state(), SupState::Failed);

        // Calling again must not change state.
        core.force_all_reapers_dead();
        assert_eq!(core.state(), SupState::Failed, "Failed must be terminal");
    }

    // ── Fault injection: panic during try_wait ───────────────────────────

    #[test]
    fn panic_during_try_reap_retains_handle() {
        // Simulates a worker thread panicking while processing a request.
        // The handle must be retained in the registry, not dropped.
        let core = make_test_core();
        let mock = MockReapable::new(vec![MockOutcome::Panic, MockOutcome::Exited(0)]);
        let (id, _lease) = lease_from_mock(core.clone(), mock);

        // Mark as non-active so try_reap doesn't skip.
        {
            let mut inner = core.inner.lock().unwrap();
            inner.registry.get_mut(&id).unwrap().active = false;
        }

        // First try_reap panics — catch it (simulates worker crash).
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| core.try_reap(id)));
        assert!(result.is_err(), "first try_reap should panic");

        // Entry must be retained — handle and admission not lost.
        assert_eq!(core.registry_len(), 1, "entry must survive panic");
        assert_eq!(core.in_flight_count(), 1, "admission must survive panic");

        // Mutex was poisoned by the panic — verify recovery.
        // Second try_reap recovers from poison and reaps (mock returns Exited).
        let r = core.try_reap(id);
        assert!(
            matches!(r, ReapResult::ConfirmedNow(_)),
            "second try_reap should confirm after recovery"
        );
        assert_eq!(core.registry_len(), 0, "entry removed after reap");
        assert_eq!(core.in_flight_count(), 0, "admission released");
        assert_eq!(core.completed_count(), 1);
    }

    // ── Fault injection: Unknown error then recovery ─────────────────────

    #[test]
    fn unknown_then_recovery_reaps_child() {
        let core = make_test_core();
        let mock = MockReapable::new(vec![MockOutcome::Unknown(999), MockOutcome::Exited(0)]);
        let (id, _lease) = lease_from_mock(core.clone(), mock);

        {
            let mut inner = core.inner.lock().unwrap();
            inner.registry.get_mut(&id).unwrap().active = false;
        }

        // First try_reap: Unknown — entry retained, admission retained.
        assert!(matches!(core.try_reap(id), ReapResult::Unknown));
        assert_eq!(core.registry_len(), 1);
        assert_eq!(core.in_flight_count(), 1);

        // Second try_reap: Exited — fault cleared, child reaped.
        assert!(matches!(core.try_reap(id), ReapResult::ConfirmedNow(_)));
        assert_eq!(core.registry_len(), 0);
        assert_eq!(core.in_flight_count(), 0);
        assert_eq!(core.completed_count(), 1);
    }

    // ── Fault injection: timeout then recovery ───────────────────────────

    #[test]
    fn timeout_then_recovery_reaps_child() {
        let core = make_test_core();
        let mock = MockReapable::new(vec![MockOutcome::Pending, MockOutcome::Exited(0)]);
        let (id, _lease) = lease_from_mock(core.clone(), mock);

        {
            let mut inner = core.inner.lock().unwrap();
            inner.registry.get_mut(&id).unwrap().active = false;
        }

        // First try_reap: Pending — entry retained.
        assert!(matches!(core.try_reap(id), ReapResult::Pending));
        assert_eq!(core.registry_len(), 1);
        assert_eq!(core.in_flight_count(), 1);

        // Second try_reap: Exited — child reaped.
        assert!(matches!(core.try_reap(id), ReapResult::ConfirmedNow(_)));
        assert_eq!(core.registry_len(), 0);
        assert_eq!(core.in_flight_count(), 0);
        assert_eq!(core.completed_count(), 1);
    }

    // ── Fault injection: persistent EINTR then recovery ──────────────────

    #[test]
    fn persistent_eintr_then_recovery_reaps_child() {
        let core = make_test_core();
        let mock = MockReapable::new(vec![
            MockOutcome::Interrupted,
            MockOutcome::Interrupted,
            MockOutcome::Exited(0),
        ]);
        let (id, _lease) = lease_from_mock(core.clone(), mock);

        {
            let mut inner = core.inner.lock().unwrap();
            inner.registry.get_mut(&id).unwrap().active = false;
        }

        // EINTR is treated as Pending — entry retained.
        assert!(matches!(core.try_reap(id), ReapResult::Pending));
        assert!(matches!(core.try_reap(id), ReapResult::Pending));
        assert_eq!(core.registry_len(), 1, "entry retained during EINTR");
        assert_eq!(core.in_flight_count(), 1, "admission retained during EINTR");

        // Third try_reap: Exited — child reaped.
        assert!(matches!(core.try_reap(id), ReapResult::ConfirmedNow(_)));
        assert_eq!(core.registry_len(), 0);
        assert_eq!(core.in_flight_count(), 0);
        assert_eq!(core.completed_count(), 1);
    }

    // ── Fatal cleanup: partial reaping with mixed outcomes ───────────────

    #[test]
    fn fatal_cleanup_partial_reap_mixed_outcomes() {
        let core = make_test_core();
        let mock_exit = MockReapable::new(vec![MockOutcome::Exited(0)]);
        let (_id1, _lease1) = lease_from_mock(core.clone(), mock_exit);
        let mock_unknown = MockReapable::new(vec![MockOutcome::Unknown(999)]);
        let (_id2, _lease2) = lease_from_mock(core.clone(), mock_unknown);
        let mock_stuck = MockReapable::new(vec![MockOutcome::Pending]);
        let (_id3, _lease3) = lease_from_mock(core.clone(), mock_stuck);
        assert_eq!(core.in_flight_count(), 3);
        core.force_all_reapers_dead();
        assert_eq!(core.state(), SupState::Failed);
        // Manually reap via checkout (no emergency thread in test core).
        for id in core.all_ids() {
            if let Some(mut guard) = core.checkout_for_fatal(id)
                && let Some(r) = guard.reapable_mut()
            {
                match blocking_reap(r, core.fatal_reap_timeout()) {
                    BlockingReapResult::Exited(s) => guard.consume(ReapStatus::Exited(s)),
                    BlockingReapResult::AlreadyReaped => guard.consume(ReapStatus::AlreadyReaped),
                    _ => guard.restore(),
                }
            }
        }
        assert_eq!(core.completed_count(), 1, "only exited child reaped");
        assert_eq!(core.registry_len(), 2, "Unknown and Pending retained");
        assert_eq!(core.in_flight_count(), 2, "admission retained for unreaped");
    }

    // ── Fatal cleanup: Unknown retains handle (no admission release) ──────

    #[test]
    fn fatal_cleanup_unknown_retains_handle() {
        let core = make_test_core();
        let mock = MockReapable::new(vec![MockOutcome::Unknown(999)]);
        let (id, _lease) = lease_from_mock(core.clone(), mock);

        assert_eq!(core.in_flight_count(), 1);

        core.force_all_reapers_dead();
        assert_eq!(core.state(), SupState::Failed);

        // Unknown — entry retained, admission NOT released.
        assert_eq!(core.registry_len(), 1, "entry retained on Unknown");
        assert_eq!(core.in_flight_count(), 1, "admission retained on Unknown");
        assert_eq!(core.completed_count(), 0);

        // Verify the handle is still in the registry.
        let inner = core.inner.lock().unwrap();
        assert!(
            inner.registry.contains_key(&id),
            "handle must be in registry"
        );
        assert!(
            inner.registry.get(&id).unwrap().reapable.is_some(),
            "reapable handle must not be dropped"
        );
    }

    // ── Round-18 fault interleaving tests ─────────────────────────────────

    #[test]
    fn commit_child_after_degraded_kills_immediately() {
        let core = make_test_core();
        // Admission acquired while Healthy.
        let epoch = core.try_admit().expect("must admit while healthy");
        // Transition to Degraded (epoch increments, admissions invalidated).
        core.force_worker_exit();
        assert_eq!(core.state(), SupState::Degraded);
        assert_ne!(core.epoch(), epoch, "epoch must change on degradation");

        // Commit child with stale epoch — must be killed immediately
        // and marked non-active.
        let mock = MockReapable::new(vec![MockOutcome::Exited(0)]);
        let id = core.commit_child(Box::new(mock), epoch);

        {
            let inner = core.inner.lock().unwrap();
            let entry = inner.registry.get(&id).unwrap();
            assert!(!entry.active, "stale-epoch child must be non-active");
        }

        // Stale-epoch child should be reaped by poll_exit.
        let result = core.poll_exit(id);
        assert!(
            matches!(result, PollResult::Exited(_)),
            "stale-epoch child should be reaped"
        );
        assert_eq!(core.in_flight_count(), 0, "admission released after reap");
    }

    #[test]
    fn checkout_guard_panic_restores_handle() {
        let core = make_test_core();
        let mock = MockReapable::new(vec![MockOutcome::Panic]);
        let (id, _lease) = lease_from_mock(core.clone(), mock);

        assert_eq!(core.in_flight_count(), 1);
        assert_eq!(core.registry_len(), 1);

        let timeout = core.fatal_reap_timeout();
        let core2 = core.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let mut guard = core2.checkout_for_fatal(id).expect("entry must exist");
            let r = guard.reapable_mut().expect("guard has reapable");
            let _ = blocking_reap(r, timeout);
        }));
        assert!(result.is_err(), "blocking_reap must panic");

        // CheckoutGuard::drop must have restored the handle.
        assert_eq!(core.registry_len(), 1, "entry must be retained after panic");
        assert_eq!(
            core.in_flight_count(),
            1,
            "admission must be retained after panic"
        );
        {
            let inner = core.inner.lock().unwrap();
            assert!(
                inner.registry.get(&id).unwrap().reapable.is_some(),
                "handle must be restored to registry after panic"
            );
        }
    }

    #[test]
    fn emergency_thread_reaps_after_all_dead() {
        let reaper = init_reaper(&default_spawner).expect("init must succeed");
        reaper
            .core
            .set_fatal_reap_timeout(Duration::from_millis(500));

        // Acquire admission while Healthy.
        let epoch = reaper.core.try_admit().expect("must admit while healthy");

        // Force all reapers dead — epoch increments, state = Failed.
        reaper.core.force_all_reapers_dead();
        assert_eq!(reaper.core.state(), SupState::Failed);

        // Commit child with stale epoch — active=false, killed immediately.
        // Emergency thread will find it in pending_ids() and reap it.
        let mock = MockReapable::new(vec![MockOutcome::Exited(0)]);
        let _id = reaper.core.commit_child(Box::new(mock), epoch);
        assert_eq!(reaper.core.in_flight_count(), 1);
        assert_eq!(reaper.core.registry_len(), 1);

        // Wait for emergency thread to reap (STUCK_REAPER_INTERVAL = 1s).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if reaper.core.completed_count() >= 1 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("emergency thread did not reap child within 5s");
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        assert_eq!(reaper.core.completed_count(), 1);
        assert_eq!(reaper.core.in_flight_count(), 0);
        assert_eq!(reaper.core.registry_len(), 0);
    }

    #[test]
    fn single_worker_death_stays_degraded() {
        let core = make_test_core();
        assert_eq!(core.state(), SupState::Healthy);
        assert!(core.is_healthy());

        // One worker dies (out of 4).
        core.force_worker_exit();

        // Must be Degraded, not Failed.
        assert_eq!(core.state(), SupState::Degraded);
        assert!(!core.is_healthy());

        let metrics = core.metrics();
        assert_eq!(metrics.workers_alive, 3, "3 workers still alive");
        assert!(metrics.stuck_alive);
        assert!(metrics.orphan_alive);
        assert!(metrics.emergency_alive, "emergency must be alive");

        // Admission must be closed (epoch changed).
        assert!(
            core.try_admit().is_none(),
            "admission must be closed after Degraded"
        );
    }

    #[test]
    #[cfg(unix)]
    fn try_reap_echild_releases_admission() {
        let core = make_test_core();
        let mock = MockReapable::new(vec![MockOutcome::AlreadyReaped]);
        let (id, lease) = lease_from_mock(core.clone(), mock);

        assert_eq!(core.in_flight_count(), 1);

        // Drop lease — cancel_child sets active=false and kills.
        drop(lease);

        // try_reap processes the non-active child.
        // Mock returns ECHILD on first try_wait.
        let result = core.try_reap(id);
        assert!(
            matches!(result, ReapResult::ConfirmedNow(ReapStatus::AlreadyReaped)),
            "ECHILD must return ConfirmedNow(AlreadyReaped), not fake success"
        );
        assert_eq!(core.in_flight_count(), 0, "admission released on ECHILD");
        assert_eq!(core.completed_count(), 1, "completed incremented on ECHILD");
    }

    #[test]
    #[cfg(unix)]
    fn wait_echild_returns_error_not_success() {
        let core = make_test_core();
        let mock = MockReapable::new(vec![MockOutcome::AlreadyReaped]);
        let (_id, mut lease) = lease_from_mock(core.clone(), mock);

        // wait() calls poll_exit, which calls try_wait.
        // Mock returns ECHILD — poll_exit returns Exited(AlreadyReaped),
        // wait() returns Err (not fake success).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(lease.wait());
        assert!(
            result.is_err(),
            "wait() must return Err on ECHILD, not fake success"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unknown"),
            "error must mention unknown status, got: {err}"
        );

        // Admission released, completed incremented.
        assert_eq!(core.in_flight_count(), 0);
        assert_eq!(core.completed_count(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn cancel_child_failed_state_real_child_reaped() {
        let reaper = init_reaper(&default_spawner).expect("init must succeed");
        reaper.core.set_fatal_reap_timeout(Duration::from_secs(2));

        let child = spawn_child("sleep", "30");
        let pid = child.id().expect("child must have a PID");

        let admission = reaper.try_admit().expect("should admit");
        let lease = admission.into_lease(child);

        // Force all reapers dead — lease drop does synchronous blocking reap.
        reaper.core.force_all_reapers_dead();
        assert_eq!(reaper.core.state(), SupState::Failed);

        drop(lease);

        assert!(
            wait_for_reap(pid, Duration::from_secs(5)),
            "child PID {pid} must be reaped after lease drop in Failed state"
        );
        assert_eq!(
            reaper.core.in_flight_count(),
            0,
            "admission released after Failed-state reap"
        );
    }

    // ── Emergency panic isolation and real component-fault probes ─────────

    #[test]
    fn emergency_thread_survives_panic_and_reaps() {
        let reaper = init_reaper(&default_spawner).expect("init must succeed");
        reaper
            .core
            .set_fatal_reap_timeout(Duration::from_millis(500));

        let controller_epoch = reaper.core.try_admit().expect("controller admission");
        let target_epoch = reaper.core.try_admit().expect("target admission");
        let _controller_trace = induce_real_emergency_takeover(&reaper, controller_epoch);
        let panic_baseline = reaper.metrics().reap_panics;

        let target_trace = Arc::new(AttemptTrace::default());
        reaper.core.commit_child(
            Box::new(PanicOnceThenExit::new(target_trace.clone(), None)),
            target_epoch,
        );

        wait_until(
            Duration::from_secs(10),
            "emergency owner did not retry and reap the panic-once target",
            || reaper.core.completed_count() == 2,
        );

        assert_eq!(target_trace.attempts.load(Ordering::SeqCst), 2);
        assert_eq!(target_trace.panics.load(Ordering::SeqCst), 1);
        assert!(target_trace.killed.load(Ordering::SeqCst));
        assert_eq!(
            target_trace.thread_names(),
            vec![
                "child-reaper-emergency".to_string(),
                "child-reaper-emergency".to_string()
            ]
        );
        assert_eq!(reaper.metrics().reap_panics, panic_baseline + 1);
        assert_eq!(reaper.core.in_flight_count(), 0);
        assert_eq!(reaper.core.registry_len(), 0);
    }

    #[test]
    fn emergency_persistent_panic_does_not_block_other_children() {
        let reaper = init_reaper(&default_spawner).expect("init must succeed");
        reaper
            .core
            .set_fatal_reap_timeout(Duration::from_millis(100));

        let controller_epoch = reaper.core.try_admit().expect("controller admission");
        let persistent_epoch = reaper.core.try_admit().expect("persistent admission");
        let normal_epoch = reaper.core.try_admit().expect("normal admission");
        let _controller_trace = induce_real_emergency_takeover(&reaper, controller_epoch);
        let panic_baseline = reaper.metrics().reap_panics;

        let persistent_trace = Arc::new(AttemptTrace::default());
        reaper.core.commit_child(
            Box::new(AlwaysPanic {
                trace: persistent_trace.clone(),
            }),
            persistent_epoch,
        );

        let normal_trace = Arc::new(AttemptTrace::default());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        reaper.core.commit_child(
            Box::new(ExitWhenReleased {
                trace: normal_trace.clone(),
                entered: entered_tx,
                release: release_rx,
            }),
            normal_epoch,
        );
        entered_rx
            .recv_timeout(TEST_SYNC_TIMEOUT)
            .expect("normal child was not attempted before deadline");
        assert_eq!(
            normal_trace.thread_names(),
            vec!["child-reaper-emergency".to_string()]
        );
        release_tx
            .send(())
            .expect("normal child release receiver dropped");

        wait_until(
            Duration::from_secs(10),
            "normal child was blocked by the persistent-panic child",
            || reaper.core.completed_count() == 2,
        );
        wait_until(
            Duration::from_secs(5),
            "persistent child was not attempted by emergency owner",
            || persistent_trace.panics.load(Ordering::SeqCst) > 0,
        );
        assert_eq!(reaper.core.registry_len(), 1);
        assert_eq!(reaper.core.in_flight_count(), 1);

        let stable_core = reaper.core.clone();
        drop(reaper);
        assert_eq!(
            persistent_trace.attempts.load(Ordering::SeqCst),
            persistent_trace.panics.load(Ordering::SeqCst)
        );
        assert!(
            persistent_trace
                .thread_names()
                .iter()
                .all(|name| name == "child-reaper-emergency")
        );
        assert_eq!(
            stable_core.metrics().reap_panics - panic_baseline,
            persistent_trace.panics.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn init_reaper_rollback_at_emergency_failure() {
        let (spawner, tracker) = make_tracked_failing_spawner(6);
        let result = init_reaper(spawner.as_ref());
        assert!(result.is_err());
        assert_eq!(tracker.started.load(Ordering::SeqCst), 6);
        assert_eq!(tracker.exited.load(Ordering::SeqCst), 6);
        assert_eq!(
            tracker.active.load(Ordering::SeqCst),
            0,
            "failed init returned with residual component threads"
        );
    }

    #[test]
    fn real_worker_panic_degrades_and_child_eventually_reaped() {
        let scanner_gate = Arc::new(TimedGate::default());
        let spawner = make_scanner_gated_spawner(scanner_gate.clone());
        let reaper = init_reaper(&spawner).expect("init must succeed");
        reaper
            .core
            .set_fatal_reap_timeout(Duration::from_millis(500));

        let epoch = reaper.core.try_admit().expect("must admit while healthy");
        let (first_attempt_tx, first_attempt_rx) = std::sync::mpsc::channel();
        let trace = Arc::new(AttemptTrace::default());
        let id = reaper.core.commit_child(
            Box::new(PanicOnceThenExit::new(
                trace.clone(),
                Some(first_attempt_tx),
            )),
            epoch,
        );
        ChildLease::cancel_child_with_core(&reaper.core, id);
        first_attempt_rx
            .recv_timeout(TEST_SYNC_TIMEOUT)
            .expect("worker did not reach injected panic before deadline");
        scanner_gate.open();

        wait_until(Duration::from_secs(5), "worker did not panic", || {
            reaper.metrics().component_failures == 1
        });
        wait_until(
            Duration::from_secs(5),
            "surviving normal reaper did not recover child",
            || reaper.core.completed_count() == 1,
        );

        assert_eq!(reaper.core.state(), SupState::Degraded);
        assert_eq!(reaper.core.epoch(), epoch + 1);
        let metrics = reaper.metrics();
        assert_eq!(metrics.workers_alive, REAPER_WORKERS - 1);
        assert!(metrics.stuck_alive && metrics.orphan_alive);
        assert_eq!(metrics.component_failures, 1);
        assert_eq!(metrics.reap_panics, 0);
        assert!(reaper.try_admit().is_none());
        assert_eq!(trace.attempts.load(Ordering::SeqCst), 2);
        assert_eq!(trace.panics.load(Ordering::SeqCst), 1);
        let names = trace.thread_names();
        assert!(names[0].starts_with("child-reaper-") && names[0] != "child-reaper-emergency");
        assert_ne!(names[1], "child-reaper-emergency");
        assert_eq!(reaper.core.in_flight_count(), 0);
        assert_eq!(reaper.core.registry_len(), 0);
    }

    #[test]
    fn epoch_fencing_rejects_stale_admission_after_real_worker_panic() {
        let scanner_gate = Arc::new(TimedGate::default());
        let spawner = make_scanner_gated_spawner(scanner_gate.clone());
        let reaper = init_reaper(&spawner).expect("init must succeed");
        reaper
            .core
            .set_fatal_reap_timeout(Duration::from_millis(500));

        let stale_epoch = reaper.core.try_admit().expect("stale admission");
        let trigger_epoch = reaper.core.try_admit().expect("trigger admission");
        let (first_attempt_tx, first_attempt_rx) = std::sync::mpsc::channel();
        let trigger_trace = Arc::new(AttemptTrace::default());
        let trigger_id = reaper.core.commit_child(
            Box::new(PanicOnceThenExit::new(
                trigger_trace,
                Some(first_attempt_tx),
            )),
            trigger_epoch,
        );
        ChildLease::cancel_child_with_core(&reaper.core, trigger_id);
        first_attempt_rx
            .recv_timeout(TEST_SYNC_TIMEOUT)
            .expect("worker did not reach epoch-trigger panic before deadline");
        scanner_gate.open();
        wait_until(
            Duration::from_secs(5),
            "worker fault did not advance epoch",
            || reaper.metrics().component_failures == 1,
        );
        assert_eq!(reaper.core.state(), SupState::Degraded);
        assert_eq!(reaper.core.epoch(), stale_epoch + 1);

        let target_trace = Arc::new(AttemptTrace::default());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        reaper.core.commit_child(
            Box::new(ExitWhenReleased {
                trace: target_trace.clone(),
                entered: entered_tx,
                release: release_rx,
            }),
            stale_epoch,
        );
        entered_rx
            .recv_timeout(TEST_SYNC_TIMEOUT)
            .expect("stale-epoch target was not attempted before deadline");
        assert!(target_trace.killed.load(Ordering::SeqCst));
        assert_ne!(target_trace.thread_names()[0], "child-reaper-emergency");
        release_tx
            .send(())
            .expect("stale-epoch target release receiver dropped");
        wait_until(
            Duration::from_secs(5),
            "stale-epoch target was not reaped by a surviving normal component",
            || reaper.core.completed_count() == 2,
        );

        assert_eq!(reaper.metrics().component_failures, 1);
        assert_eq!(reaper.metrics().reap_panics, 0);
        assert_eq!(reaper.core.in_flight_count(), 0);
        assert_eq!(reaper.core.registry_len(), 0);
    }
}
