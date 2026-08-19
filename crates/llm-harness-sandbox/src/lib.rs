//! Sandbox backends for `llm-harness-runtime`.
//!
//! This crate provides `ExecutionEnv` and `Sandbox` implementations with
//! varying isolation levels:
//!
//! - **`os`** (default, all platforms): No isolation. Direct OS filesystem
//!   and shell access. Suitable for development and testing.
//! - **`bwrap`** (Linux): Production isolation via `cap-std` capability-based
//!   filesystem access and `bubblewrap --unshare-all` namespace isolation.
//! - **`seatbelt`** (macOS): Production isolation via macOS `sandbox-exec`
//!   seatbelt policy with comprehensive host-path denylist and lexical path
//!   containment checks with symlink resolution.
//!
//! The `shell_runner` module is shared infrastructure used by all backends:
//! child process reaping, output draining, and UTF-8 boundary handling.

mod lifecycle;
pub mod shell_runner;

#[cfg(feature = "os")]
pub mod os;

#[cfg(all(feature = "bwrap", target_os = "linux"))]
pub mod bwrap;

#[cfg(all(feature = "seatbelt", target_os = "macos"))]
pub mod seatbelt;

// Re-export shared infrastructure at crate root.
pub use shell_runner::{
    ChildLease, ChildReaper, ReaperMetrics, SpawnAdmission, drain_child_output,
    drain_child_output_guarded,
};

#[cfg(feature = "os")]
pub use os::{OsEnv, OsEnvFactory, OsEnvSandbox};

#[cfg(all(feature = "bwrap", target_os = "linux"))]
pub use bwrap::{BwrapEnv, BwrapSandbox};

/// Stub — `BwrapSandbox` is only available on Linux.
#[cfg(all(feature = "bwrap", not(target_os = "linux")))]
#[derive(Debug)]
pub struct BwrapSandbox;

#[cfg(all(feature = "bwrap", not(target_os = "linux")))]
impl BwrapSandbox {
    /// Returns an error on non-Linux platforms (fail-closed).
    pub fn new(_config: llm_harness_platform::sandbox::SandboxConfig) -> anyhow::Result<Self> {
        Err(anyhow::anyhow!("BwrapSandbox is only available on Linux"))
    }
}

#[cfg(all(feature = "seatbelt", target_os = "macos"))]
pub use seatbelt::SeatbeltSandbox;

/// Stub — `SeatbeltSandbox` is only available on macOS.
#[cfg(all(feature = "seatbelt", not(target_os = "macos")))]
#[derive(Debug)]
pub struct SeatbeltSandbox {
    _priv: (),
}

#[cfg(all(feature = "seatbelt", not(target_os = "macos")))]
impl SeatbeltSandbox {
    /// Returns an error on non-macOS platforms (fail-closed).
    pub fn new(_config: llm_harness_platform::sandbox::SandboxConfig) -> anyhow::Result<Self> {
        Err(anyhow::anyhow!(
            "SeatbeltSandbox is only available on macOS"
        ))
    }
}
