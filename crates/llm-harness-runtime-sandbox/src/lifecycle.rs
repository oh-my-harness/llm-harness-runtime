//! Shared lifecycle and path helpers used by `BwrapEnv` and `SeatbeltEnv`.
//!
//! Both backends implement the same lifecycle state machine (Running →
//! Resetting → Running / Tainted, and Running → ShuttingDown → Stopped)
//! and the same lexical path normalization. Extracting these here avoids
//! duplication and keeps the two backends in sync.

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

/// Sandbox lifecycle state, tracked atomically to coordinate shutdown.
///
/// - `Running`: normal operation; new work is admitted.
/// - `Resetting`: reset is in progress; new work is rejected so reset can
///   acquire the lock without racing against a newly-admitted command.
///   After cleanup, the state transitions back to `Running`.
/// - `ShuttingDown`: shutdown has begun; new work is rejected, active
///   commands are cancelled and killed.
/// - `Stopped`: shutdown is complete; no further operations are possible.
/// - `Tainted`: reset cleanup failed or panicked mid-mutation; the sandbox
///   is unusable and must be shut down and recreated. New work is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum LifecycleState {
    Running = 0,
    Resetting = 1,
    ShuttingDown = 2,
    Stopped = 3,
    Tainted = 4,
}

impl LifecycleState {
    pub(crate) fn from_atomic(v: u8) -> Self {
        match v {
            0 => LifecycleState::Running,
            1 => LifecycleState::Resetting,
            2 => LifecycleState::ShuttingDown,
            3 => LifecycleState::Stopped,
            _ => LifecycleState::Tainted,
        }
    }
}

/// Maximum time `shutdown()` waits for an active shell command to terminate
/// after sending kill before proceeding with workspace deletion.
pub(crate) const SHUTDOWN_KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Lexically normalize a path: resolve `.` and `..` components without
/// touching the filesystem.
pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// Shared lifecycle state machine operations backed by an `AtomicU8`.
///
/// Both `BwrapEnv` and `SeatbeltEnv` store their lifecycle as an `AtomicU8`
/// and use the same CAS transitions. This struct encapsulates the atomic,
/// so the methods don't need to be duplicated on each env type.
#[derive(Debug)]
pub(crate) struct LifecycleAtomic(AtomicU8);

impl LifecycleAtomic {
    pub(crate) fn new() -> Self {
        Self(AtomicU8::new(LifecycleState::Running as u8))
    }

    pub(crate) fn load(&self) -> LifecycleState {
        LifecycleState::from_atomic(self.0.load(Ordering::SeqCst))
    }

    /// Transition to `ShuttingDown`. Returns `true` if this call performed
    /// the transition. Accepts both `Running` and `Resetting` as valid
    /// starting states so shutdown can interrupt an in-progress reset.
    pub(crate) fn begin_shutdown(&self) -> bool {
        // Try Running -> ShuttingDown first (common case).
        if self
            .0
            .compare_exchange(
                LifecycleState::Running as u8,
                LifecycleState::ShuttingDown as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            return true;
        }
        // Try Tainted -> ShuttingDown (shutdown of a tainted sandbox).
        if self
            .0
            .compare_exchange(
                LifecycleState::Tainted as u8,
                LifecycleState::ShuttingDown as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            return true;
        }
        // Try Resetting -> ShuttingDown (shutdown during reset).
        self.0
            .compare_exchange(
                LifecycleState::Resetting as u8,
                LifecycleState::ShuttingDown as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Transition `Running -> Resetting`. Returns `true` if this call
    /// performed the transition.
    pub(crate) fn begin_reset(&self) -> bool {
        self.0
            .compare_exchange(
                LifecycleState::Running as u8,
                LifecycleState::Resetting as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Transition `Resetting -> Running`. Returns `true` on success.
    pub(crate) fn finish_reset(&self) -> bool {
        self.0
            .compare_exchange(
                LifecycleState::Resetting as u8,
                LifecycleState::Running as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Transition `Resetting -> Tainted`. Returns `true` if this call
    /// performed the transition.
    pub(crate) fn taint(&self) -> bool {
        self.0
            .compare_exchange(
                LifecycleState::Resetting as u8,
                LifecycleState::Tainted as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Transition to `Stopped`.
    pub(crate) fn finish_shutdown(&self) {
        self.0
            .store(LifecycleState::Stopped as u8, Ordering::SeqCst);
    }
}
