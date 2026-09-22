//! Firecracker VM lifecycle primitives for the remote sandbox gateway.

pub mod config;
pub use config::{FirecrackerConfig, FirecrackerConfigError};
#[cfg(target_os = "linux")]
mod cgroup;
#[cfg(target_os = "linux")]
pub mod image;
#[cfg(target_os = "linux")]
pub use image::{FirecrackerImage, FirecrackerImageAllocator, FirecrackerImageError};
#[cfg(target_os = "linux")]
mod jail;
#[cfg(target_os = "linux")]
pub mod jailer;
#[cfg(target_os = "linux")]
pub use jailer::{
    FirecrackerJailerCgroupVersion, FirecrackerJailerConfig, FirecrackerJailerConfigError,
};
#[cfg(target_os = "linux")]
pub mod lifecycle;
#[cfg(target_os = "linux")]
pub use lifecycle::{FirecrackerLifecycle, FirecrackerLifecycleConfig, FirecrackerLifecycleError};
pub mod process;
#[cfg(target_os = "linux")]
pub use process::FirecrackerProcess;
pub use process::{FirecrackerProcessConfig, FirecrackerProcessError};
