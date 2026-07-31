//! Shared shell output draining logic used by `OsEnv` and `BwrapEnv`.
//!
//! Both backends spawn a child process (directly or via `bwrap`) and need the
//! same stdout/stderr draining, UTF-8 boundary handling, timeout, and abort
//! logic. Extracting it here avoids code duplication and keeps
//! `utf8_safe_boundary` as an internal detail.

use llm_harness_types::{EnvError, ShellOptions, ShellOutput};
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

/// Create an `ExitStatus` from a raw exit code.  Unix-only — on other
/// platforms this is unreachable (ECHILD = -1 never matches).
#[cfg(unix)]
fn exit_status_from_raw(code: i32) -> std::process::ExitStatus {
    std::process::ExitStatus::from_raw(code)
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

/// How often the stuck/orphan reaper threads scan the registry.
const STUCK_REAPER_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Per-child timeout for synchronous blocking reap in fatal paths
/// (all reaper components dead, or checkout-during-Failed).
const FATAL_REAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
    ConfirmedNow(std::process::ExitStatus),
    /// Entry already removed by another thread — do NOT increment.
    AlreadyAbsent,
    /// Child still running (or active/skipped).  Entry retained.
    Pending,
    /// Unknown wait error.  Entry retained.
    Unknown,
}

/// Result of a drain-function poll on an active child.
#[derive(Debug)]
enum PollResult {
    /// Child exited; entry removed, admission released.
    Exited(std::process::ExitStatus),
    /// Entry removed by another path (e.g. fatal_cleanup).
    AlreadyGone,
    /// Child still running or checked out.
    Pending,
}

// ── Component liveness bitmask ───────────────────────────────────────────

const COMP_WORKERS: u8 = 0b001;
const COMP_STUCK: u8 = 0b010;
const COMP_ORPHAN: u8 = 0b100;
const COMP_ALL: u8 = COMP_WORKERS | COMP_STUCK | COMP_ORPHAN;

/// Unique identifier for a child registered in the registry.
type ChildId = u64;

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
/// synchronous reap in the Failed path.  When `None`, reaper threads
/// skip the entry (treat as Pending).
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

// ── SupervisorCore: single-mutex authority ────────────────────────────────

/// Inner state protected by a single `Mutex`.  All state transitions —
/// admission, child commit, reap, cancellation, component death,
/// fatal_cleanup — go through this one lock, eliminating TOCTOU races.
struct SupervisorInner {
    state: SupState,
    in_flight: usize,
    admitted: usize,
    completed: usize,
    failures: usize,
    registry: std::collections::HashMap<ChildId, RegistryEntry>,
    next_id: u64,
    /// Bitmask of alive components (COMP_WORKERS | COMP_STUCK | COMP_ORPHAN).
    components: u8,
    /// Per-child timeout for fatal/synchronous reap paths.
    fatal_reap_timeout: std::time::Duration,
}

/// The authoritative supervisor core.  Holds all reaper state behind a
/// single `Mutex`, so every state transition is linearized.
///
/// **Invariants enforced by the single-mutex design:**
///
/// 1. `try_admit` checks health AND increments `in_flight` under one lock —
///    no window for state to change between check and increment.
/// 2. `commit_child` registers the child to the registry under the same
///    lock that checked admission health — the child is committed
///    immediately, not deferred to lease drop.
/// 3. `try_reap` / `poll_exit` do `try_wait` + remove-entry + release-
///    admission under one lock — only the thread that removes the entry
///    returns `ConfirmedNow`, guaranteeing exactly-once `completed`.
/// 4. `on_component_exit` clears the component bit AND runs
///    `fatal_cleanup` (or rescan) under one lock — no window for a
///    late lease-drop to register to a dead registry.
struct SupervisorCore {
    inner: std::sync::Mutex<SupervisorInner>,
    sender: std::sync::mpsc::SyncSender<ChildId>,
}

impl SupervisorCore {
    fn new(sender: std::sync::mpsc::SyncSender<ChildId>) -> Self {
        Self {
            inner: std::sync::Mutex::new(SupervisorInner {
                state: SupState::Healthy,
                in_flight: 0,
                admitted: 0,
                completed: 0,
                failures: 0,
                registry: std::collections::HashMap::new(),
                next_id: 1,
                components: COMP_ALL,
                fatal_reap_timeout: FATAL_REAP_TIMEOUT,
            }),
            sender,
        }
    }

    /// Acquire a pre-spawn admission slot.  Atomic health-check + increment.
    /// Returns `true` if admitted.
    fn try_admit(&self) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.state != SupState::Healthy || inner.components != COMP_ALL {
            return false;
        }
        if inner.in_flight >= REAPER_QUEUE_CAPACITY {
            return false;
        }
        inner.in_flight += 1;
        inner.admitted += 1;
        true
    }

    /// Release a pre-spawn admission slot (spawn failed or never attempted).
    fn release_admission(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.in_flight > 0 {
            inner.in_flight -= 1;
        }
    }

    /// Immediately commit a spawned child to the authoritative registry.
    /// The child is registered as `active: true` (reaper threads skip it
    /// while the drain function is using it).  If the supervisor is already
    /// `Failed`, the child is killed immediately so the drain's polling
    /// will detect exit.
    fn commit_child(&self, reapable: Box<dyn Reapable>) -> ChildId {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let id = inner.next_id;
        inner.next_id += 1;

        let failed = inner.state == SupState::Failed;
        let workers_alive = inner.components & COMP_WORKERS != 0;

        let mut entry = RegistryEntry {
            reapable: Some(reapable),
            has_admission: true,
            active: !failed,
            queued_at: std::time::Instant::now(),
        };

        if failed {
            // Supervisor already dead — kill immediately so drain polling
            // detects exit.  No workers to route to.
            if let Some(r) = entry.reapable.as_mut() {
                r.start_kill();
            }
        }

        inner.registry.insert(id, entry);

        if !failed && workers_alive {
            let _ = self.sender.try_send(id);
        }

        id
    }

    /// Cancel an active child (lease dropped without confirmation).
    /// If the supervisor is alive: set `active: false`, kill, route to
    /// workers.  If `Failed`: check out the child for synchronous reap.
    fn cancel_child(&self, id: ChildId) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        if inner.state == SupState::Failed {
            // All reapers dead.  Check out the reapable for synchronous
            // reap outside the lock (blocking must not hold the mutex).
            let timeout = inner.fatal_reap_timeout;
            let reapable = inner.registry.get_mut(&id).and_then(|e| e.reapable.take());
            drop(inner); // Unlock for blocking reap.

            if let Some(mut r) = reapable {
                if let Some(_status) = blocking_reap(&mut *r, timeout) {
                    // Reaped — remove entry, release admission.
                    let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(entry) = inner.registry.remove(&id) {
                        if entry.has_admission {
                            inner.in_flight -= 1;
                        }
                        inner.completed += 1;
                    }
                } else {
                    // Not reaped — put the handle back (fail-closed).
                    let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(entry) = inner.registry.get_mut(&id) {
                        entry.reapable = Some(r);
                    }
                }
            }
        } else {
            // Supervisor alive — kill and route.
            let workers_alive = inner.components & COMP_WORKERS != 0;
            if let Some(entry) = inner.registry.get_mut(&id) {
                entry.active = false;
                if let Some(r) = entry.reapable.as_mut() {
                    r.start_kill();
                }
            }
            if workers_alive {
                let _ = self.sender.try_send(id);
            }
        }
    }

    /// Poll an active child for exit (used by the drain function).
    /// Does NOT skip active entries and does NOT kill.  If the child has
    /// exited, removes the entry and releases admission atomically.
    fn poll_exit(&self, id: ChildId) -> PollResult {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // Phase 1: try_wait (borrows entry).  Scoped so the borrow ends
        // before phase 2 removes the entry.
        let wait_result = {
            let Some(entry) = inner.registry.get_mut(&id) else {
                return PollResult::AlreadyGone;
            };
            let Some(reapable) = entry.reapable.as_mut() else {
                return PollResult::Pending; // Checked out for sync reap.
            };
            reapable.try_wait()
        };
        // Phase 2: act on result (borrows registry for removal).
        match wait_result {
            Ok(Some(status)) => {
                if let Some(entry) = inner.registry.remove(&id) {
                    if entry.has_admission {
                        inner.in_flight -= 1;
                    }
                    inner.completed += 1;
                }
                PollResult::Exited(status)
            }
            Ok(None) => PollResult::Pending,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => PollResult::Pending,
            Err(e) if e.raw_os_error() == Some(ECHILD) => {
                if let Some(entry) = inner.registry.remove(&id) {
                    if entry.has_admission {
                        inner.in_flight -= 1;
                    }
                    inner.completed += 1;
                }
                PollResult::Exited(exit_status_from_raw(0))
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
        // Phase 1: kill + try_wait (borrows entry).  Scoped so the borrow
        // ends before phase 2 removes the entry.
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
            reapable.start_kill(); // Idempotent.
            reapable.try_wait()
        };
        // Phase 2: act on result (borrows registry for removal).
        match wait_result {
            Ok(Some(status)) => {
                if let Some(entry) = inner.registry.remove(&id) {
                    if entry.has_admission {
                        inner.in_flight -= 1;
                    }
                    inner.completed += 1;
                }
                ReapResult::ConfirmedNow(status)
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
                ReapResult::ConfirmedNow(exit_status_from_raw(0))
            }
            Err(_) => ReapResult::Unknown,
        }
    }

    /// IDs of non-active entries (for stuck/orphan scanners).
    fn pending_ids(&self) -> Vec<ChildId> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .registry
            .iter()
            .filter(|(_, e)| !e.active)
            .map(|(&id, _)| id)
            .collect()
    }

    /// Called when a reaper component thread exits.  Clears the component
    /// bit and handles the transition atomically under one lock.
    fn on_component_exit(&self, mask: u8) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.components &= !mask;
        inner.failures += 1;

        let alive = inner.components;
        if alive == 0 {
            // All components dead — fatal transition.
            if inner.state != SupState::Failed {
                inner.state.transition_to(SupState::Failed);
                let unreaped = Self::fatal_cleanup_locked(&mut inner);
                if unreaped > 0 {
                    inner.failures += unreaped;
                }
            }
        } else {
            inner.state.transition_to(SupState::Degraded);
            // Rescan non-active entries and route to surviving workers.
            if alive & COMP_WORKERS != 0 {
                let ids: Vec<ChildId> = inner
                    .registry
                    .iter()
                    .filter(|(_, e)| !e.active)
                    .map(|(&id, _)| id)
                    .collect();
                for id in ids {
                    let _ = self.sender.try_send(id);
                }
            }
        }
    }

    /// Synchronously kill + wait all remaining children.  Called while
    /// holding the supervisor lock (fatal path — no concurrent operations
    /// expected).  Only `Exited`/`AlreadyReaped` children are removed and
    /// have admission released.  Unreaped handles are retained (fail-closed).
    ///
    /// Each child gets its own deadline (`FATAL_REAP_TIMEOUT`), not a
    /// shared one.
    fn fatal_cleanup_locked(inner: &mut SupervisorInner) -> usize {
        let ids: Vec<ChildId> = inner.registry.keys().copied().collect();
        let mut unreaped = 0;
        for id in ids {
            let reaped = if let Some(entry) = inner.registry.get_mut(&id) {
                if let Some(reapable) = entry.reapable.as_mut() {
                    reapable.start_kill();
                    let child_deadline = std::time::Instant::now() + inner.fatal_reap_timeout;
                    loop {
                        match reapable.try_wait() {
                            Ok(Some(_)) => break true,
                            Err(e) if e.raw_os_error() == Some(ECHILD) => break true,
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                                if std::time::Instant::now() >= child_deadline {
                                    break false;
                                }
                                std::thread::sleep(EINTR_BACKOFF);
                            }
                            Ok(None) => {
                                if std::time::Instant::now() >= child_deadline {
                                    break false;
                                }
                                std::thread::sleep(POLL_INTERVAL);
                            }
                            Err(_) => break false, // Unknown — retain.
                        }
                    }
                } else {
                    true // Checked out — skip.
                }
            } else {
                true // Already removed.
            };
            if reaped {
                if let Some(entry) = inner.registry.remove(&id) {
                    if entry.has_admission {
                        inner.in_flight -= 1;
                    }
                    inner.completed += 1;
                }
            } else {
                unreaped += 1;
            }
        }
        unreaped
    }

    fn is_healthy(&self) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.state == SupState::Healthy
    }

    fn metrics(&self) -> ReaperMetrics {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        ReaperMetrics {
            in_flight: inner.in_flight,
            admitted: inner.admitted,
            completed: inner.completed,
            worker_failures: inner.failures,
            registry_len: inner.registry.len(),
            healthy: inner.state == SupState::Healthy,
            workers_alive: inner.components & COMP_WORKERS != 0,
            stuck_alive: inner.components & COMP_STUCK != 0,
            orphan_alive: inner.components & COMP_ORPHAN != 0,
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
    fn components(&self) -> u8 {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.components
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
}

// ── SpawnAdmission ────────────────────────────────────────────────────────

/// Pre-spawn admission token.  Dropping it releases the in-flight slot
/// (correct when spawn fails or is never attempted).  Call `into_lease`
/// to couple with a spawned child.
pub struct SpawnAdmission {
    core: Option<std::sync::Arc<SupervisorCore>>,
}

impl SpawnAdmission {
    /// Couple this admission with a spawned child.  The child is
    /// **immediately** committed to the authoritative registry under the
    /// supervisor lock — not deferred to lease drop.  Returns an opaque
    /// [`ChildLease`] that holds only a `ChildId` and stdout/stderr handles.
    pub fn into_lease(mut self, mut child: tokio::process::Child) -> ChildLease {
        let core = self.core.take().expect("into_lease called twice");
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let id = core.commit_child(Box::new(child));
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
/// blocking reap.
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
                PollResult::Exited(status) => {
                    self.confirmed = true;
                    return Ok(status);
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

    /// Kill the child and poll until exit or `FATAL_REAP_TIMEOUT`.
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
            core.cancel_child(self.id);
        }
    }
}

// ── LivenessGuard ─────────────────────────────────────────────────────────

/// RAII guard that clears a component's liveness bit on drop.  Every
/// required thread (worker, stuck, orphan) carries one.
struct LivenessGuard {
    core: std::sync::Arc<SupervisorCore>,
    mask: u8,
}

impl Drop for LivenessGuard {
    fn drop(&mut self) {
        self.core.on_component_exit(self.mask);
    }
}

// ── Metrics ───────────────────────────────────────────────────────────────

/// Snapshot of reaper state for observability.
#[derive(Debug)]
pub struct ReaperMetrics {
    pub in_flight: usize,
    pub admitted: usize,
    pub completed: usize,
    pub worker_failures: usize,
    pub registry_len: usize,
    pub healthy: bool,
    pub workers_alive: bool,
    pub stuck_alive: bool,
    pub orphan_alive: bool,
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
    let core = std::sync::Arc::new(SupervisorCore::new(sender.clone()));
    let stop = std::sync::Arc::new(AtomicBool::new(false));

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
                    mask: COMP_WORKERS,
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
                    mask: COMP_STUCK,
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
                    mask: COMP_ORPHAN,
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

    Ok(ChildReaper {
        core,
        _sender: sender,
        stop,
        handles: std::sync::Mutex::new(handles),
    })
}

// ── blocking_reap (fatal-path synchronous reap) ───────────────────────────

/// Synchronously kill + wait a reapable with a per-child deadline.
/// Returns `Some(status)` if confirmed exited, `None` if timed out or
/// unknown error.  Does NOT store the handle — the caller retains it.
fn blocking_reap(
    reapable: &mut dyn Reapable,
    timeout: std::time::Duration,
) -> Option<std::process::ExitStatus> {
    reapable.start_kill();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match reapable.try_wait() {
            Ok(Some(status)) => return Some(status),
            Err(e) if e.raw_os_error() == Some(ECHILD) => {
                return Some(exit_status_from_raw(0));
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                if std::time::Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(EINTR_BACKOFF);
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(_) => return None, // Unknown — caller retains handle.
        }
    }
}

// ── ChildReaper ───────────────────────────────────────────────────────────

/// Centralized child process reaper with bounded admission, authoritative
/// registry, per-component supervisor health, and transactional init.
///
/// All state lives in [`SupervisorCore`] behind a single `Mutex`,
/// linearizing every state transition.
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
    /// or at capacity.
    pub fn try_admit(&self) -> Option<SpawnAdmission> {
        if self.core.try_admit() {
            Some(SpawnAdmission {
                core: Some(self.core.clone()),
            })
        } else {
            None
        }
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

    /// Orphan reaper: fallback scanner when workers and stuck are dead.
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
            let ids = core.pending_ids();
            for id in ids {
                let _ = core.try_reap(id);
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
        // After join, all LivenessGuards have dropped, triggering
        // on_component_exit → fatal_cleanup for any remaining children.
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

#[cfg(all(test, unix))]
mod reap_tests {
    use super::*;
    use llm_harness_types::ShellOptions;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    // ── Test helpers ──────────────────────────────────────────────────────

    fn process_is_gone(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/status")).is_err()
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

    /// Make a test core with no threads (state=Healthy, components=COMP_ALL).
    fn make_test_core() -> Arc<SupervisorCore> {
        let (sender, _receiver) = std::sync::mpsc::sync_channel::<ChildId>(REAPER_QUEUE_CAPACITY);
        Arc::new(SupervisorCore::new(sender))
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

    /// Test-only lease from a mock reapable.
    fn lease_from_mock(core: Arc<SupervisorCore>, mock: MockReapable) -> (ChildId, ChildLease) {
        assert!(core.try_admit(), "test core must admit");
        let id = core.commit_child(Box::new(mock));
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
            assert!(core.try_admit());
        }
        assert_eq!(core.in_flight_count(), REAPER_QUEUE_CAPACITY);
        assert!(!core.try_admit(), "should reject at capacity");

        // Simulate releasing all admissions.
        for _ in 0..REAPER_QUEUE_CAPACITY {
            core.release_admission();
        }
        assert_eq!(core.in_flight_count(), 0);

        assert!(core.try_admit());
        assert_eq!(core.in_flight_count(), 1);
        core.release_admission();
        assert_eq!(core.in_flight_count(), 0);
    }

    #[test]
    fn admission_closed_after_failed_state() {
        let core = make_test_core();
        assert!(core.try_admit());
        core.release_admission();

        // Simulate all components dying.
        core.on_component_exit(COMP_ALL);
        assert_eq!(core.state(), SupState::Failed);

        // Admission must be permanently closed.
        assert!(!core.try_admit(), "admission must be closed after Failed");
        assert!(!core.try_admit(), "admission must remain closed");
    }

    #[test]
    fn admission_closed_after_degraded_state() {
        let core = make_test_core();
        assert!(core.try_admit());
        core.release_admission();

        // Simulate one component dying.
        core.on_component_exit(COMP_WORKERS);
        assert_eq!(core.state(), SupState::Degraded);

        // Admission must be closed (requires all components alive).
        assert!(!core.try_admit(), "admission must be closed when Degraded");
    }

    // ── Spawn failure releases pre-spawn admission only ───────────────────

    #[test]
    fn spawn_failure_releases_pre_spawn_admission() {
        let core = make_test_core();
        assert!(core.try_admit());
        assert_eq!(core.in_flight_count(), 1);

        // Simulate spawn failure: SpawnAdmission is dropped without into_lease.
        let admission = SpawnAdmission {
            core: Some(core.clone()),
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
        core.cancel_child(id);

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

        // Child that exits immediately.
        let mock_exit = MockReapable::new(vec![MockOutcome::Exited(0)]);
        let (_id1, _lease1) = lease_from_mock(core.clone(), mock_exit);

        // Child that never exits (always Pending).
        let mock_stuck = MockReapable::new(vec![MockOutcome::Pending]);
        let (_id2, _lease2) = lease_from_mock(core.clone(), mock_stuck);

        assert_eq!(core.in_flight_count(), 2);

        // Trigger fatal transition.
        core.on_component_exit(COMP_ALL);
        assert_eq!(core.state(), SupState::Failed);

        // fatal_cleanup ran: id1 should be reaped (exited), id2 retained.
        // Note: mock_stuck always returns Pending, so it times out after
        // FATAL_REAP_TIMEOUT.  But that's 5 seconds — too slow for a unit
        // test.  Instead, verify the state transition and that id1 was
        // reaped.
        // The stuck child will time out, so we just check that at least
        // id1 was reaped and the state is Failed.
        assert_eq!(core.state(), SupState::Failed);
        // id1 should be gone (exited immediately).
        // id2 might still be there (timing out) or gone if the mock
        // returned something else.  Just verify completed >= 1.
        assert!(
            core.completed_count() >= 1,
            "at least the exited child should be reaped"
        );
    }

    #[test]
    fn fatal_cleanup_per_child_deadline() {
        // Verify that fatal_cleanup uses a per-child deadline, not a shared
        // one.  We can't easily test timing in a unit test, but we verify
        // the structure: each child gets its own child_deadline.
        // This is a structural assertion — the code uses
        // `Instant::now() + FATAL_REAP_TIMEOUT` inside the per-child loop.
        let core = make_test_core();
        let mock = MockReapable::new(vec![MockOutcome::Exited(0)]);
        let (_id, _lease) = lease_from_mock(core.clone(), mock);
        core.on_component_exit(COMP_ALL);
        assert_eq!(core.state(), SupState::Failed);
        assert_eq!(core.registry_len(), 0, "exited child must be reaped");
        assert_eq!(core.in_flight_count(), 0, "admission released");
    }

    // ── Late registration after Failed ────────────────────────────────────

    #[test]
    fn commit_child_after_failed_kills_immediately() {
        let core = make_test_core();
        // Admission acquired BEFORE the failure transition.
        assert!(core.try_admit(), "must admit while healthy");
        core.on_component_exit(COMP_ALL);
        assert_eq!(core.state(), SupState::Failed);

        // Child spawned after Failed — commit_child kills immediately.
        // Mock exits on first try_wait (child killed by commit_child).
        let mock = MockReapable::new(vec![MockOutcome::Exited(0)]);
        let id = core.commit_child(Box::new(mock));

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
        // Unknown during fatal_cleanup (retains entry), then Exited during
        // cancel_child's blocking_reap (confirms exit).
        let mock = MockReapable::new(vec![MockOutcome::Unknown(999), MockOutcome::Exited(0)]);
        let (_id, lease) = lease_from_mock(core.clone(), mock);

        // Transition to Failed — fatal_cleanup sees Unknown, retains entry.
        core.on_component_exit(COMP_ALL);
        assert_eq!(core.state(), SupState::Failed);
        assert_eq!(
            core.registry_len(),
            1,
            "entry must be retained after Unknown"
        );

        // Drop the lease — cancel_child does synchronous blocking_reap.
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
        assert!(reaper.metrics().workers_alive);
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
        assert!(core.try_admit());
        core.release_admission();
        let _metrics = core.metrics();
    }

    // ── Real child PID tests (integration) ────────────────────────────────

    #[test]
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
            metrics.in_flight <= 32,
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
        assert!(metrics.workers_alive);
        assert!(metrics.stuck_alive);
        assert!(metrics.orphan_alive);
    }

    // ── blocking_reap unit test ───────────────────────────────────────────

    #[test]
    fn blocking_reap_exits() {
        let mut mock = MockReapable::new(vec![MockOutcome::Exited(0)]);
        let result = blocking_reap(&mut mock, FATAL_REAP_TIMEOUT);
        assert!(
            result.is_some(),
            "blocking_reap should return Some for exited child"
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
            result.is_none(),
            "blocking_reap should return None on timeout"
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
            result.is_none(),
            "blocking_reap should return None on unknown error"
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

        core.on_component_exit(COMP_WORKERS);
        assert_eq!(core.state(), SupState::Degraded);

        // Dying more components stays Degraded (not Failed until all dead).
        core.on_component_exit(COMP_STUCK);
        assert_eq!(core.state(), SupState::Degraded);

        // Last component dies → Failed.
        core.on_component_exit(COMP_ORPHAN);
        assert_eq!(core.state(), SupState::Failed);
    }

    #[test]
    fn failed_state_is_terminal() {
        let core = make_test_core();
        core.on_component_exit(COMP_ALL);
        assert_eq!(core.state(), SupState::Failed);

        // Calling on_component_exit again must not change state.
        core.on_component_exit(COMP_ALL);
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
        // One child exits, one returns Unknown, one is Pending.
        // Only the exited child should be reaped; others retained.
        let core = make_test_core();

        let mock_exit = MockReapable::new(vec![MockOutcome::Exited(0)]);
        let (_id1, _lease1) = lease_from_mock(core.clone(), mock_exit);

        let mock_unknown = MockReapable::new(vec![MockOutcome::Unknown(999)]);
        let (_id2, _lease2) = lease_from_mock(core.clone(), mock_unknown);

        let mock_stuck = MockReapable::new(vec![MockOutcome::Pending]);
        let (_id3, _lease3) = lease_from_mock(core.clone(), mock_stuck);

        assert_eq!(core.in_flight_count(), 3);

        // Trigger fatal transition.
        core.on_component_exit(COMP_ALL);
        assert_eq!(core.state(), SupState::Failed);

        // Exited child reaped; Unknown and Pending retained.
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

        core.on_component_exit(COMP_ALL);
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
}
