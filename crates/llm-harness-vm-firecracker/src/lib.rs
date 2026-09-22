//! Firecracker VM lifecycle primitives for the remote sandbox gateway.

pub mod config;
pub use config::{FirecrackerConfig, FirecrackerConfigError};
#[cfg(target_os = "linux")]
pub mod image;
#[cfg(target_os = "linux")]
pub use image::{FirecrackerImage, FirecrackerImageAllocator, FirecrackerImageError};
pub mod process;
#[cfg(target_os = "linux")]
pub use process::FirecrackerProcess;
pub use process::{FirecrackerProcessConfig, FirecrackerProcessError};
