//! Firecracker VM lifecycle primitives for the remote sandbox gateway.

pub mod config;
pub use config::{FirecrackerConfig, FirecrackerConfigError};
pub mod process;
#[cfg(target_os = "linux")]
pub use process::FirecrackerProcess;
pub use process::{FirecrackerProcessConfig, FirecrackerProcessError};
