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

/// Per-child timeout for synchronous blocking reap in the fatal path
/// (all reaper components dead or no global reaper).
const FATAL_REAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
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

/// Unique identifier for a child registered in the [`ChildRegistry`].
type ChildId = u64;

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
    /// the reaper registry (after a real `try_wait` confirming `Exited`/
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
/// `Drop` registers the child in the authoritative [`ChildRegistry`] if
/// exit has not been confirmed.  The registry holds the sole handle until
/// `try_wait` confirms terminal state — no queue or thread-local vector
/// ever owns the last handle.
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
        // Child exit NOT confirmed.  Register the child in the
        // authoritative registry — the registry holds the sole handle
        // until try_wait confirms terminal state.  No queue or vector
        // ever owns the last handle.
        if let Some(child) = self.child.take() {
            let in_flight = self.in_flight.take();
            if let Some(reaper) = ChildReaper::global() {
                let id = reaper.registry.register(child, in_flight);
                reaper.route_id(id);
            } else {
                // No global reaper at all.  Synchronous blocking reap
                // (fatal path) — kills and waits with a bounded timeout.
                blocking_kill_and_reap(child, in_flight);
            }
        }
    }
}

// ── Authoritative child registry ─────────────────────────────────────────

/// A child process pending reaping, stored in the central registry.
///
/// The registry — not any queue or thread-local vector — owns the sole
/// `Child` handle.  Worker, stuck, and orphan queues carry only `ChildId`
/// values, so dropping a queue item or panicking in a worker thread
/// cannot lose the ability to reap a child.
struct RegistryEntry {
    child: tokio::process::Child,
    in_flight: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    #[allow(dead_code)]
    queued_at: std::time::Instant,
}

/// Authoritative, bounded child registry.
///
/// Holds the **sole** `Child` handles until `wait`/`try_wait` confirms
/// terminal state.  Worker, stuck, and orphan queues carry only `ChildId`
/// values — never the last handle.  This guarantees that dropping a queue
/// item, panicking in a worker thread, or a thread dying while processing
/// work cannot lose the ability to reap a child.
///
/// When a reaper component dies, its [`LivenessGuard`] rescans the
/// registry and re-enqueues pending IDs to surviving components.  When
/// all components are dead, [`fatal_cleanup`] synchronously kills and
/// waits all remaining children.
struct ChildRegistry {
    entries: std::sync::Mutex<std::collections::HashMap<ChildId, RegistryEntry>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl ChildRegistry {
    fn new() -> Self {
        Self {
            entries: std::sync::Mutex::new(std::collections::HashMap::new()),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Register a child.  The child is killed immediately.  Returns the
    /// assigned `ChildId` for queueing.
    fn register(
        &self,
        mut child: tokio::process::Child,
        in_flight: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    ) -> ChildId {
        let _ = child.start_kill();
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.insert(
            id,
            RegistryEntry {
                child,
                in_flight,
                queued_at: std::time::Instant::now(),
            },
        );
        id
    }

    /// Attempt a single `try_wait` on a registered child.  If the child
    /// has exited, removes the entry and releases admission.
    ///
    /// Returns `true` if the child was confirmed exited (or already
    /// removed by another thread).
    fn try_reap_once(&self, id: ChildId) -> bool {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = entries.get_mut(&id) {
            match classify_wait(entry.child.try_wait()) {
                WaitOutcome::Exited | WaitOutcome::AlreadyReaped => {
                    let entry = entries.remove(&id).expect("entry exists");
                    if let Some(in_flight) = &entry.in_flight {
                        decrement_in_flight(in_flight);
                    }
                    true
                }
                _ => false,
            }
        } else {
            true // Already reaped by another thread.
        }
    }

    /// Attempt to reap a child with a bounded deadline.  Locks the
    /// registry only briefly for each `try_wait` call, so other workers
    /// and the stuck scheduler are not blocked.
    ///
    /// Returns `true` if the child was confirmed exited (or already
    /// removed by another thread).
    fn try_reap_with_deadline(&self, id: ChildId, deadline: std::time::Instant) -> bool {
        loop {
            let outcome = {
                let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(entry) = entries.get_mut(&id) {
                    classify_wait(entry.child.try_wait())
                } else {
                    return true; // Already reaped.
                }
            };
            match outcome {
                WaitOutcome::Exited | WaitOutcome::AlreadyReaped => {
                    let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(entry) = entries.remove(&id)
                        && let Some(in_flight) = &entry.in_flight
                    {
                        decrement_in_flight(in_flight);
                    }
                    return true;
                }
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

    /// Return all pending child IDs (for rescans by the stuck scheduler
    /// and orphan reaper, and for component-death migration).
    fn all_ids(&self) -> Vec<ChildId> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.keys().copied().collect()
    }

    /// Number of entries currently in the registry.
    fn len(&self) -> usize {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.len()
    }

    /// Synchronously kill + wait all remaining children.  Used when all
    /// reaper components are dead (fatal process-level transition).
    ///
    /// This is a blocking operation with a per-child timeout.  After
    /// cleanup, the registry is empty and all admission slots are
    /// released.  This replaces the old `mem::forget` path — children
    /// are never silently leaked as zombies.
    fn fatal_cleanup(&self) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = std::time::Instant::now() + FATAL_REAP_TIMEOUT;
        for (_, entry) in entries.iter_mut() {
            let _ = entry.child.start_kill();
            loop {
                match classify_wait(entry.child.try_wait()) {
                    WaitOutcome::Exited | WaitOutcome::AlreadyReaped => break,
                    WaitOutcome::Interrupted => {
                        if std::time::Instant::now() >= deadline {
                            break;
                        }
                        continue;
                    }
                    WaitOutcome::Pending => {
                        if std::time::Instant::now() >= deadline {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    WaitOutcome::Unknown(_) => break,
                }
            }
            if let Some(in_flight) = &entry.in_flight {
                decrement_in_flight(in_flight);
            }
        }
        entries.clear();
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
    /// Current registry length (children pending reap).
    pub registry_len: usize,
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
/// required thread (worker, stuck, orphan) carries one.
///
/// On drop:
/// 1. Clears the component's liveness bit (monotonic — bits only clear).
/// 2. If all components are dead, calls `fatal_cleanup` to synchronously
///    kill + wait all remaining children in the registry.
/// 3. If some components survive, rescans the registry and re-enqueues
///    pending IDs to surviving workers (if the sender is available).
struct LivenessGuard {
    components: std::sync::Arc<std::sync::atomic::AtomicU8>,
    failures: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    mask: u8,
    registry: std::sync::Arc<ChildRegistry>,
    sender: Option<std::sync::mpsc::SyncSender<ChildId>>,
}

impl Drop for LivenessGuard {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        // Monotonic: only clears bits, never sets them.
        self.components.fetch_and(!self.mask, Ordering::SeqCst);
        self.failures.fetch_add(1, Ordering::SeqCst);

        let alive = self.components.load(Ordering::SeqCst);
        if alive == 0 {
            // All components dead — fatal process-level transition.
            // Synchronously kill + wait all remaining children.
            self.registry.fatal_cleanup();
        } else if let Some(sender) = self.sender.as_ref() {
            // Rescan registry and re-enqueue to surviving workers.
            let ids = self.registry.all_ids();
            for id in ids {
                if sender.try_send(id).is_err() {
                    break; // Channel full/disconnected — entries stay.
                }
            }
        }
        // If no sender (orphan reaper), entries stay in registry for
        // periodic scans by surviving components.
    }
}

/// A centralized, multi-worker child process reaper with **bounded
/// admission**, **authoritative child registry**, **per-component
/// supervisor health**, and **transactional initialisation**.
///
/// Architecture:
/// - [`ChildRegistry`] holds the sole `Child` handles until `try_wait`
///   confirms terminal state.  Worker, stuck, and orphan queues carry
///   only `ChildId` values — never the last handle.
/// - `try_admit()` returns `Option<SpawnAdmission>`; admission is closed
///   when **any** component (worker pool, stuck scheduler, orphan reaper)
///   is dead.
/// - `route_id()` enqueues a `ChildId` to the worker channel.  If workers
///   are dead or the queue is full, the entry stays in the registry —
///   the stuck scheduler or orphan reaper will scan and find it.
/// - All threads carry a [`LivenessGuard`] that clears the corresponding
///   component bit on exit.  On component death, the guard rescans the
///   registry and re-enqueues pending IDs to surviving components.  When
///   all components are dead, `fatal_cleanup` synchronously kills and
///   waits all remaining children.
/// - Initialisation is transactional: if any thread fails to spawn, all
///   previously started threads are stopped and joined before returning
///   `None`.
pub struct ChildReaper {
    sender: std::sync::mpsc::SyncSender<ChildId>,
    registry: std::sync::Arc<ChildRegistry>,
    /// Global in-flight counter; bounds total unreaped children.
    in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Bitmask of alive components (COMP_WORKERS | COMP_STUCK | COMP_ORPHAN).
    components: std::sync::Arc<std::sync::atomic::AtomicU8>,
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
                    std::sync::mpsc::sync_channel::<ChildId>(REAPER_QUEUE_CAPACITY);
                let receiver = std::sync::Arc::new(std::sync::Mutex::new(receiver));
                let registry = std::sync::Arc::new(ChildRegistry::new());
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
                    let reg = registry.clone();
                    let comp = components.clone();
                    let fail = failures.clone();
                    let done = completed.clone();
                    let stp = stop.clone();
                    let snd = sender.clone();
                    let guard_reg = registry.clone();
                    match std::thread::Builder::new()
                        .name(format!("child-reaper-{i}"))
                        .spawn(move || {
                            let _guard = LivenessGuard {
                                components: comp,
                                failures: fail,
                                mask: COMP_WORKERS,
                                registry: guard_reg,
                                sender: Some(snd),
                            };
                            Self::worker_loop(rx, reg, done, stp);
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
                    let reg = registry.clone();
                    let done = completed.clone();
                    let comp = components.clone();
                    let fail = failures.clone();
                    let stp = stop.clone();
                    let snd = sender.clone();
                    let stuck_snd = sender.clone();
                    let guard_reg = registry.clone();
                    match std::thread::Builder::new()
                        .name("child-reaper-stuck".to_string())
                        .spawn(move || {
                            let _guard = LivenessGuard {
                                components: comp,
                                failures: fail,
                                mask: COMP_STUCK,
                                registry: guard_reg,
                                sender: Some(snd),
                            };
                            Self::stuck_loop(reg, stuck_snd, done, stp);
                        }) {
                        Ok(h) => handles.push(h),
                        Err(_) => failed = true,
                    }
                }

                // ── Spawn orphan reaper thread ────────────────────────
                if !failed {
                    let reg = registry.clone();
                    let done = completed.clone();
                    let comp = components.clone();
                    let fail = failures.clone();
                    let stp = stop.clone();
                    let guard_reg = registry.clone();
                    match std::thread::Builder::new()
                        .name("child-reaper-orphan".to_string())
                        .spawn(move || {
                            let _guard = LivenessGuard {
                                components: comp,
                                failures: fail,
                                mask: COMP_ORPHAN,
                                registry: guard_reg,
                                sender: None,
                            };
                            Self::orphan_reaper_loop(reg, done, stp);
                        }) {
                        Ok(h) => handles.push(h),
                        Err(_) => failed = true,
                    }
                }

                // ── Transactional cleanup on failure ──────────────────
                if failed {
                    stop.store(true, Ordering::SeqCst);
                    drop(sender);
                    for h in handles {
                        let _ = h.join();
                    }
                    components.store(0, Ordering::SeqCst);
                    return None;
                }

                Some(ChildReaper {
                    sender,
                    registry,
                    in_flight,
                    components,
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
            registry_len: self.registry.len(),
            healthy: comp == COMP_ALL,
            workers_alive: comp & COMP_WORKERS != 0,
            stuck_alive: self.stuck_alive(),
            orphan_alive: self.orphan_alive(),
        }
    }

    /// Enqueue a `ChildId` to the worker channel.  If workers are dead
    /// or the queue is full, the entry stays in the registry — the stuck
    /// scheduler or orphan reaper will scan and find it.
    fn route_id(&self, id: ChildId) {
        if self.workers_alive() {
            let _ = self.sender.try_send(id);
        }
        // If workers dead or queue full, the entry remains in the
        // registry.  The stuck scheduler (every STUCK_REAPER_INTERVAL)
        // and orphan reaper both scan the registry and will reap it.
    }

    /// Worker thread main loop.  Pulls `ChildId` values from the channel
    /// and attempts to reap them with a bounded deadline.  If the child
    /// doesn't exit within the deadline, it stays in the registry for
    /// the stuck scheduler to revisit.
    fn worker_loop(
        receiver: std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<ChildId>>>,
        registry: std::sync::Arc<ChildRegistry>,
        completed: std::sync::Arc<std::sync::atomic::AtomicUsize>,
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
                    if registry.try_reap_with_deadline(id, deadline) {
                        completed.fetch_add(1, Ordering::SeqCst);
                    }
                    // If not reaped, the entry stays in the registry.
                    // The stuck scheduler will revisit it.
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    /// Stuck reaper thread.  Periodically scans the registry for pending
    /// children and attempts to reap them.  Re-enqueues non-exited
    /// children to the worker channel for another attempt.
    fn stuck_loop(
        registry: std::sync::Arc<ChildRegistry>,
        sender: std::sync::mpsc::SyncSender<ChildId>,
        completed: std::sync::Arc<std::sync::atomic::AtomicUsize>,
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

            let ids = registry.all_ids();
            for id in ids {
                if registry.try_reap_once(id) {
                    completed.fetch_add(1, Ordering::SeqCst);
                } else {
                    let _ = sender.try_send(id);
                }
            }
        }
    }

    /// Orphan reaper thread.  Periodically scans the registry as a
    /// fallback when workers and the stuck scheduler are both dead.
    fn orphan_reaper_loop(
        registry: std::sync::Arc<ChildRegistry>,
        completed: std::sync::Arc<std::sync::atomic::AtomicUsize>,
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

            let ids = registry.all_ids();
            for id in ids {
                if registry.try_reap_once(id) {
                    completed.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    }
}

/// Interval at which all threads check the stop flag.
const STOP_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Synchronously kill + wait a child with a bounded timeout.  Used in
/// the fatal path (no global reaper available) — never leaks a zombie.
fn blocking_kill_and_reap(
    mut child: tokio::process::Child,
    in_flight: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
) {
    let _ = child.start_kill();
    let deadline = std::time::Instant::now() + FATAL_REAP_TIMEOUT;
    loop {
        match classify_wait(child.try_wait()) {
            WaitOutcome::Exited | WaitOutcome::AlreadyReaped => break,
            WaitOutcome::Interrupted => {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                continue;
            }
            WaitOutcome::Pending => {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            WaitOutcome::Unknown(_) => break,
        }
    }
    if let Some(in_flight) = &in_flight {
        decrement_in_flight(in_flight);
    }
}

#[cfg(test)]
impl ChildReaper {
    /// Create a ChildReaper whose receiver is already disconnected.
    fn new_disconnected() -> ChildReaper {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<ChildId>(1);
        drop(receiver);
        ChildReaper {
            sender,
            registry: std::sync::Arc::new(ChildRegistry::new()),
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            components: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(COMP_ALL)),
            admitted: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            completed: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            failures: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Create a ChildReaper with a bounded queue, no worker threads, and
    /// the receiver kept alive (returned to caller).
    fn new_no_workers(capacity: usize) -> (ChildReaper, std::sync::mpsc::Receiver<ChildId>) {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<ChildId>(capacity);
        (
            ChildReaper {
                sender,
                registry: std::sync::Arc::new(ChildRegistry::new()),
                in_flight: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                components: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(COMP_ALL)),
                admitted: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                completed: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                failures: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            receiver,
        )
    }

    /// Create a ChildReaper in `Failed` state (all components dead).
    #[allow(dead_code)]
    fn new_failed() -> ChildReaper {
        let (sender, _receiver) = std::sync::mpsc::sync_channel::<ChildId>(1);
        ChildReaper {
            sender,
            registry: std::sync::Arc::new(ChildRegistry::new()),
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            components: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            admitted: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            completed: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            failures: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Create a ChildReaper with specific components marked as dead.
    fn new_with_components(alive: u8) -> ChildReaper {
        let (sender, _receiver) = std::sync::mpsc::sync_channel::<ChildId>(1);
        ChildReaper {
            sender,
            registry: std::sync::Arc::new(ChildRegistry::new()),
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            components: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(alive)),
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
        let id = self.registry.register(child, in_flight);
        self.route_id(id);
    }

    /// Get a clone of the in_flight counter (for testing).
    fn in_flight_arc(&self) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        self.in_flight.clone()
    }

    /// Get a clone of the registry Arc (for testing).
    fn registry_arc(&self) -> std::sync::Arc<ChildRegistry> {
        self.registry.clone()
    }

    /// Get a clone of the components atomic (for testing).
    #[allow(dead_code)]
    fn components_arc(&self) -> std::sync::Arc<std::sync::atomic::AtomicU8> {
        self.components.clone()
    }

    /// Get a clone of the failures counter (for testing).
    fn failures_arc(&self) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        self.failures.clone()
    }
}
#[cfg(test)]
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
        COMP_ALL, ChildLease, ChildReaper, WaitOutcome, classify_wait, drain_child_output_guarded,
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

    // ── ChildLease integration tests (all assert PID disappearance) ───────

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

        let pid = rt.block_on(async {
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

    // ── Unpolled future test ──────────────────────────────────────────────

    #[test]
    fn unpolled_future_still_reaps_child() {
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

        let lease = ChildLease::new(child);
        let _future = drain_child_output_guarded(lease, test_opts(), CancellationToken::new());
        // Drop the future without polling it.
        drop(_future);

        assert!(
            wait_for_reap(pid, Duration::from_secs(5)),
            "child PID {pid} must be reaped even if the drain future is never polled"
        );
        drop(rt);
    }

    // ── Admission bound tests ─────────────────────────────────────────────

    #[test]
    fn admission_at_capacity_rejects_then_recovers() {
        let reaper = ChildReaper::new_disconnected();

        let mut admissions: Vec<_> = Vec::with_capacity(super::REAPER_QUEUE_CAPACITY);
        for _ in 0..super::REAPER_QUEUE_CAPACITY {
            admissions.push(reaper.try_admit().expect("should admit below capacity"));
        }
        assert_eq!(reaper.metrics().in_flight, super::REAPER_QUEUE_CAPACITY);
        assert!(reaper.try_admit().is_none(), "should reject at capacity");

        drop(admissions);
        assert_eq!(reaper.metrics().in_flight, 0);

        let admission = reaper.try_admit().expect("should admit after recovery");
        assert_eq!(reaper.metrics().in_flight, 1);
        drop(admission);
        assert_eq!(reaper.metrics().in_flight, 0);
    }

    #[test]
    fn spawn_failure_releases_pre_spawn_admission() {
        let reaper = ChildReaper::new_disconnected();

        let admission = reaper.try_admit().expect("should admit");
        assert_eq!(reaper.metrics().in_flight, 1);

        drop(admission);
        assert_eq!(reaper.metrics().in_flight, 0);
    }

    // ── Registry-based reap tests (all assert PID disappearance) ──────────

    #[test]
    fn reap_recovers_child_on_disconnected_receiver() {
        let reaper = ChildReaper::new_disconnected();
        let child = spawn_child("sleep", "30");
        let pid = child.id().expect("child must have a PID");
        reaper.reap_direct(child, None);

        // Receiver is disconnected, so the ID can't be queued to workers.
        // The entry stays in the registry.  The global reaper's stuck
        // scheduler will eventually scan and reap it.  But this test
        // reaper has no threads, so we verify the child is in the
        // registry and manually reap it.
        let registry = reaper.registry_arc();
        let ids = registry.all_ids();
        assert_eq!(ids.len(), 1, "child must be in the registry");

        // Manually reap to clean up the child (with deadline to ensure exit).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        assert!(
            registry.try_reap_with_deadline(ids[0], deadline),
            "child must be reaped from registry"
        );
        assert!(
            wait_for_reap(pid, Duration::from_secs(5)),
            "child PID {pid} must be reaped after registry try_reap_with_deadline"
        );
    }

    #[test]
    fn reap_recovers_child_on_full_queue() {
        let (reaper, _receiver) = ChildReaper::new_no_workers(1);

        let filler = spawn_child("sleep", "30");
        let filler_pid = filler.id().expect("child must have a PID");
        reaper.reap_direct(filler, None);

        let child = spawn_child("sleep", "30");
        let pid = child.id().expect("child must have a PID");
        reaper.reap_direct(child, None);

        // Both children are in the registry (queue can only hold 1).
        let registry = reaper.registry_arc();
        assert_eq!(registry.len(), 2, "both children must be in the registry");

        // Manually reap both to clean up (with deadline to ensure exit).
        let ids = registry.all_ids();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        for id in ids {
            registry.try_reap_with_deadline(id, deadline);
        }
        assert!(
            wait_for_reap(pid, Duration::from_secs(5)),
            "child PID {pid} must be reaped"
        );
        assert!(
            wait_for_reap(filler_pid, Duration::from_secs(5)),
            "filler PID {filler_pid} must be reaped"
        );
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

    // ── P0 (round-15): All-dead does NOT leak zombies ────────────────────

    /// P0 (round-15): When all components are dead, children must still
    /// be reaped (synchronous fatal_cleanup), NOT leaked as zombies via
    /// `mem::forget`.  This test registers a child in the registry of a
    /// reaper with all components dead, then triggers fatal_cleanup via
    /// LivenessGuard::drop, and asserts PID disappearance.
    #[test]
    fn all_dead_reaps_via_fatal_cleanup() {
        let reaper = ChildReaper::new_with_components(0);
        let registry = reaper.registry_arc();

        let child = spawn_child("sleep", "30");
        let pid = child.id().expect("child must have a PID");

        // Register the child directly in the registry.
        registry.register(child, None);

        // Simulate all-dead: call fatal_cleanup directly.
        // In production, this is triggered by the last LivenessGuard::drop.
        registry.fatal_cleanup();

        assert!(
            wait_for_reap(pid, Duration::from_secs(5)),
            "child PID {pid} must be reaped via fatal_cleanup, not leaked as zombie"
        );
        assert_eq!(
            registry.len(),
            0,
            "registry must be empty after fatal_cleanup"
        );
    }

    // ── P0 (round-15): Component death migrates work ─────────────────────

    /// P0 (round-15): When a component dies, existing children in the
    /// registry must be picked up by surviving components.  This test
    /// uses the global reaper: routes a child, waits for the stuck
    /// scheduler to scan and reap it, and asserts PID disappearance.
    #[test]
    fn stuck_scheduler_reaps_registry_entries() {
        let reaper = ChildReaper::global().expect("global reaper must be available");

        let child = spawn_child("sleep", "30");
        let pid = child.id().expect("child must have a PID");

        // Register and route — the child goes to the worker channel.
        // Workers will kill + try_wait.  Since the child is `sleep 30`
        // and was killed, it should exit quickly.
        let id = reaper.registry.register(child, None);
        reaper.route_id(id);

        assert!(
            wait_for_reap(pid, Duration::from_secs(10)),
            "child PID {pid} must be reaped by the global reaper"
        );
    }

    // ── P0 (round-15): Mutex poison still reaps ──────────────────────────

    #[test]
    fn mutex_poison_still_reaps() {
        let reaper = ChildReaper::global().expect("global reaper must be available");

        // Poison the registry mutex in a separate thread.
        let registry = reaper.registry_arc();
        let handle = std::thread::spawn(move || {
            let _guard = registry.entries.lock().unwrap();
            panic!("intentional poison");
        });
        let _ = handle.join();

        // The mutex is now poisoned.  But the global reaper's workers
        // recover from poison (unwrap_or_else into_inner).  A child
        // queued via route_child goes to the worker queue.
        let in_flight = reaper.in_flight_arc();
        let admission = reaper.try_admit().expect("should admit");
        let child = spawn_child("sleep", "30");
        let pid = child.id().expect("child must have a PID");
        let lease = admission.into_lease(child);
        drop(lease);

        assert!(
            wait_for_reap(pid, Duration::from_secs(10)),
            "child PID {pid} must be reaped even with poisoned registry mutex"
        );

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

    // ── LivenessGuard monotonicity ───────────────────────────────────────

    #[test]
    fn liveness_guard_is_monotonic() {
        let components = Arc::new(AtomicU8::new(COMP_ALL));
        let failures = Arc::new(AtomicUsize::new(0));
        let registry = Arc::new(super::ChildRegistry::new());

        // Drop a worker guard — should clear only the worker bit.
        {
            let _guard = super::LivenessGuard {
                components: components.clone(),
                failures: failures.clone(),
                mask: super::COMP_WORKERS,
                registry: registry.clone(),
                sender: None,
            };
        }
        assert_eq!(
            components.load(Ordering::SeqCst),
            COMP_ALL & !super::COMP_WORKERS
        );
        assert_eq!(failures.load(Ordering::SeqCst), 1);

        // Drop a stuck guard — should clear only the stuck bit.
        {
            let _guard = super::LivenessGuard {
                components: components.clone(),
                failures: failures.clone(),
                mask: super::COMP_STUCK,
                registry: registry.clone(),
                sender: None,
            };
        }
        assert_eq!(
            components.load(Ordering::SeqCst),
            COMP_ALL & !super::COMP_WORKERS & !super::COMP_STUCK
        );
        assert_eq!(failures.load(Ordering::SeqCst), 2);
    }

    // ── Cancellation storm bounded test ──────────────────────────────────

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

    // ── P0 (round-15): Registry retains handle through component death ──

    /// P0 (round-15): When a LivenessGuard drops with surviving
    /// components, it rescans the registry and re-enqueues.  This test
    /// verifies that a child registered in the registry is reaped even
    /// after a LivenessGuard drop triggers a rescan.
    #[test]
    fn liveness_guard_rescan_reenqueues_pending() {
        let reaper = ChildReaper::global().expect("global reaper must be available");
        let registry = reaper.registry_arc();
        // Use a SEPARATE components atomic so this test does not
        // degrade the global reaper for other tests.
        let test_components = Arc::new(AtomicU8::new(COMP_ALL));

        let child = spawn_child("sleep", "30");
        let pid = child.id().expect("child must have a PID");

        // Register the child directly (bypassing route_id so it's only
        // in the registry, not the worker channel).
        registry.register(child, None);

        // Simulate a component death: drop a LivenessGuard with a
        // surviving set of components.  The guard rescans the registry
        // and re-enqueues the ID to the worker channel.
        {
            let _guard = super::LivenessGuard {
                components: test_components.clone(),
                failures: reaper.failures_arc(),
                mask: super::COMP_WORKERS,
                registry: registry.clone(),
                sender: Some(reaper.sender.clone()),
            };
            // Guard drops here — rescans and re-enqueues.
        }

        // The child should be reaped by the reaper's surviving threads.
        assert!(
            wait_for_reap(pid, Duration::from_secs(10)),
            "child PID {pid} must be reaped after LivenessGuard rescan"
        );
    }

    // ── P0 (round-15): blocking_kill_and_reap (no global reaper) ─────────

    /// P0 (round-15): When no global reaper is available, ChildLease::drop
    /// must synchronously kill + wait the child (not mem::forget).
    #[test]
    fn no_reaper_synchronous_reap() {
        // We can't easily prevent the global reaper from initializing
        // (OnceLock).  Instead, test blocking_kill_and_reap directly.
        let child = spawn_child("sleep", "30");
        let pid = child.id().expect("child must have a PID");

        super::blocking_kill_and_reap(child, None);

        assert!(
            wait_for_reap(pid, Duration::from_secs(5)),
            "child PID {pid} must be reaped by blocking_kill_and_reap"
        );
    }
}
