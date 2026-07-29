#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::{BwrapEnv, BwrapSandbox};

/// Stub — `BwrapSandbox` is only available on Linux.
#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub struct BwrapSandbox;

#[cfg(not(target_os = "linux"))]
impl BwrapSandbox {
    /// Returns an error on non-Linux platforms (fail-closed).
    pub fn new(
        _config: llm_harness_runtime::platform::sandbox::SandboxConfig,
    ) -> anyhow::Result<Self> {
        Err(anyhow::anyhow!("BwrapSandbox is only available on Linux"))
    }
}
