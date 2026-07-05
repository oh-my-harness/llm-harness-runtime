#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub use macos::SeatbeltSandbox;

/// Stub — `SeatbeltSandbox` is only available on macOS.
#[cfg(not(target_os = "macos"))]
pub struct SeatbeltSandbox;

#[cfg(not(target_os = "macos"))]
impl SeatbeltSandbox {
    /// Panics on non-macOS platforms.
    pub fn new(_config: llm_harness_runtime::platform::sandbox::SandboxConfig) -> Self {
        panic!("SeatbeltSandbox is only available on macOS")
    }
}
