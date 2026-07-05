#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::BwrapSandbox;

/// Stub — `BwrapSandbox` is only available on Linux.
#[cfg(not(target_os = "linux"))]
pub struct BwrapSandbox;

#[cfg(not(target_os = "linux"))]
impl BwrapSandbox {
    /// Panics on non-Linux platforms.
    pub fn new(_config: llm_harness_runtime::platform::sandbox::SandboxConfig) -> Self {
        panic!("BwrapSandbox is only available on Linux")
    }
}
