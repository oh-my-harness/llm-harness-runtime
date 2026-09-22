//! Firecracker VM lifecycle composition.

use std::fmt::Display;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{FirecrackerConfig, FirecrackerConfigError};
use crate::image::{
    FirecrackerImage, FirecrackerImageAllocator, FirecrackerImageError, open_directory,
    open_regular_file,
};
use crate::process::{
    FirecrackerProcess, FirecrackerProcessConfig, FirecrackerProcessConfigError,
    FirecrackerProcessError,
};

const ROOTFS_FILE_NAME: &str = "rootfs.ext4";
const API_SOCKET_FILE_NAME: &str = "firecracker.sock";
const CONFIG_FILE_NAME: &str = "firecracker.json";
const VSOCK_SOCKET_FILE_NAME: &str = "guest-agent.sock";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct FirecrackerLifecycleError {
    message: String,
}

impl FirecrackerLifecycleError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl From<FirecrackerImageError> for FirecrackerLifecycleError {
    fn from(error: FirecrackerImageError) -> Self {
        Self::new(error.to_string())
    }
}

impl From<FirecrackerConfigError> for FirecrackerLifecycleError {
    fn from(error: FirecrackerConfigError) -> Self {
        Self::new(error.to_string())
    }
}

impl From<FirecrackerProcessConfigError> for FirecrackerLifecycleError {
    fn from(error: FirecrackerProcessConfigError) -> Self {
        Self::new(error.to_string())
    }
}

impl From<FirecrackerProcessError> for FirecrackerLifecycleError {
    fn from(error: FirecrackerProcessError) -> Self {
        Self::new(error.to_string())
    }
}

impl From<std::io::Error> for FirecrackerLifecycleError {
    fn from(error: std::io::Error) -> Self {
        Self::new(format!("firecracker lifecycle io error: {error}"))
    }
}

#[derive(Debug, Clone)]
pub struct FirecrackerLifecycleConfig {
    binary_path: PathBuf,
    kernel_image_path: PathBuf,
    base_image_path: PathBuf,
    run_directory: PathBuf,
    vcpu_count: u32,
    mem_size_mib: u32,
    vsock_guest_cid: u32,
    boot_args: String,
    rootfs_read_only: bool,
    readiness_timeout: Duration,
}

impl FirecrackerLifecycleConfig {
    pub fn new(
        binary_path: impl Into<PathBuf>,
        kernel_image_path: impl Into<PathBuf>,
        base_image_path: impl Into<PathBuf>,
        run_directory: impl Into<PathBuf>,
        vcpu_count: u32,
        mem_size_mib: u32,
        vsock_guest_cid: u32,
    ) -> Self {
        Self {
            binary_path: binary_path.into(),
            kernel_image_path: kernel_image_path.into(),
            base_image_path: base_image_path.into(),
            run_directory: run_directory.into(),
            vcpu_count,
            mem_size_mib,
            vsock_guest_cid,
            boot_args: String::new(),
            rootfs_read_only: false,
            readiness_timeout: Duration::from_secs(5),
        }
    }

    pub fn with_boot_args(mut self, boot_args: impl Into<String>) -> Self {
        self.boot_args = boot_args.into();
        self
    }

    pub fn with_rootfs_read_only(mut self, read_only: bool) -> Self {
        self.rootfs_read_only = read_only;
        self
    }

    pub fn with_readiness_timeout(mut self, timeout: Duration) -> Self {
        self.readiness_timeout = timeout;
        self
    }

    pub fn rootfs_path(&self) -> PathBuf {
        self.run_directory.join(ROOTFS_FILE_NAME)
    }

    pub fn api_socket_path(&self) -> PathBuf {
        self.run_directory.join(API_SOCKET_FILE_NAME)
    }

    pub fn config_file_path(&self) -> PathBuf {
        self.run_directory.join(CONFIG_FILE_NAME)
    }

    pub fn vsock_socket_path(&self) -> PathBuf {
        self.run_directory.join(VSOCK_SOCKET_FILE_NAME)
    }

    pub fn validate(&self) -> Result<(), FirecrackerLifecycleError> {
        self.validate_input_paths()?;
        self.validate_resources()?;
        self.validate_run_directory()?;
        self.validate_host_files()?;
        self.validate_artifact_paths_are_absent()?;
        Ok(())
    }

    fn validate_input_paths(&self) -> Result<(), FirecrackerLifecycleError> {
        validate_host_path("binary", &self.binary_path)?;
        validate_host_path("kernel image", &self.kernel_image_path)?;
        validate_host_path("base image", &self.base_image_path)?;
        validate_host_path("run directory", &self.run_directory)?;
        if self.run_directory == Path::new("/") {
            return Err(FirecrackerLifecycleError::new(
                "run directory must not be the filesystem root",
            ));
        }

        let inputs = [
            ("binary", &self.binary_path),
            ("kernel image", &self.kernel_image_path),
            ("base image", &self.base_image_path),
        ];
        for (left_index, (left_label, left_path)) in inputs.iter().enumerate() {
            for (right_label, right_path) in inputs.iter().skip(left_index + 1) {
                if left_path == right_path {
                    return Err(FirecrackerLifecycleError::new(format!(
                        "{left_label} and {right_label} paths must be different"
                    )));
                }
            }
        }

        let artifacts = [
            ("rootfs", self.rootfs_path()),
            ("API socket", self.api_socket_path()),
            ("config file", self.config_file_path()),
            ("vsock socket", self.vsock_socket_path()),
        ];
        for (input_label, input_path) in inputs {
            for (artifact_label, artifact_path) in &artifacts {
                if input_path == artifact_path {
                    return Err(FirecrackerLifecycleError::new(format!(
                        "{input_label} path must not be the {artifact_label} path"
                    )));
                }
            }
        }
        Ok(())
    }

    fn validate_resources(&self) -> Result<(), FirecrackerLifecycleError> {
        if self.vcpu_count == 0 {
            return Err(FirecrackerLifecycleError::new(
                "vcpu count must be positive",
            ));
        }
        if self.mem_size_mib == 0 {
            return Err(FirecrackerLifecycleError::new(
                "memory size must be positive",
            ));
        }
        if self.vsock_guest_cid < 3 {
            return Err(FirecrackerLifecycleError::new(
                "vsock guest CID must be at least 3",
            ));
        }
        if self.readiness_timeout.is_zero() {
            return Err(FirecrackerLifecycleError::new(
                "readiness timeout must be positive",
            ));
        }
        Ok(())
    }

    fn validate_run_directory(&self) -> Result<(), FirecrackerLifecycleError> {
        open_directory(&self.run_directory).map_err(|error| {
            FirecrackerLifecycleError::new(format!("invalid run directory: {error}"))
        })?;
        self.process_config().validate()?;
        self.vm_config(&self.rootfs_path()).validate()?;
        Ok(())
    }

    fn validate_host_files(&self) -> Result<(), FirecrackerLifecycleError> {
        let binary = open_regular_file(&self.binary_path)
            .map_err(|error| contextual_error("binary", error))?;
        let binary_metadata = binary.metadata()?;
        if binary_metadata.len() == 0 {
            return Err(FirecrackerLifecycleError::new("binary must not be empty"));
        }
        if binary_metadata.permissions().mode() & 0o111 == 0 {
            return Err(FirecrackerLifecycleError::new("binary must be executable"));
        }
        for (label, path) in [
            ("kernel image", &self.kernel_image_path),
            ("base image", &self.base_image_path),
        ] {
            let file = open_regular_file(path).map_err(|error| contextual_error(label, error))?;
            if file.metadata()?.len() == 0 {
                return Err(FirecrackerLifecycleError::new(format!(
                    "{label} must not be empty"
                )));
            }
        }
        Ok(())
    }

    fn validate_artifact_paths_are_absent(&self) -> Result<(), FirecrackerLifecycleError> {
        for (label, path) in [
            ("rootfs", self.rootfs_path()),
            ("API socket", self.api_socket_path()),
            ("config file", self.config_file_path()),
            ("vsock socket", self.vsock_socket_path()),
        ] {
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {
                    return Err(FirecrackerLifecycleError::new(format!(
                        "{label} path already exists"
                    )));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn process_config(&self) -> FirecrackerProcessConfig {
        FirecrackerProcessConfig::new(
            self.binary_path.clone(),
            self.api_socket_path(),
            self.config_file_path(),
        )
    }

    fn vm_config(&self, rootfs_path: &Path) -> FirecrackerConfig {
        FirecrackerConfig::new(
            self.kernel_image_path.clone(),
            rootfs_path,
            self.vsock_socket_path(),
            self.vcpu_count,
            self.mem_size_mib,
            self.vsock_guest_cid,
        )
        .with_boot_args(self.boot_args.clone())
        .with_rootfs_read_only(self.rootfs_read_only)
    }
}

pub struct FirecrackerLifecycle {
    process: FirecrackerProcess,
    image: FirecrackerImage,
}

impl FirecrackerLifecycle {
    pub async fn start(
        config: &FirecrackerLifecycleConfig,
    ) -> Result<Self, FirecrackerLifecycleError> {
        config.validate()?;

        let mut image = FirecrackerImageAllocator::new(&config.base_image_path)
            .allocate(config.rootfs_path())?;
        let vm_config = config.vm_config(image.path());
        let process_config = config.process_config();
        let mut process = match FirecrackerProcess::start(&process_config, &vm_config) {
            Ok(process) => process,
            Err(error) => {
                let image_error = image.remove().err();
                return Err(startup_error(error, None, image_error));
            }
        };

        if let Err(error) = process.wait_ready(config.readiness_timeout).await {
            let stop_error = process.stop().await.err();
            let image_error = image.remove().err();
            return Err(startup_error(error, stop_error, image_error));
        }

        Ok(Self { process, image })
    }

    pub async fn stop(&mut self) -> Result<(), FirecrackerLifecycleError> {
        let process_result = self.process.stop().await;
        let image_result = self.image.remove();
        if let Err(process_error) = process_result {
            let image_error = image_result.err();
            return Err(stop_error(process_error, image_error));
        }
        if let Err(image_error) = image_result {
            return Err(FirecrackerLifecycleError::new(format!(
                "firecracker image cleanup failed: {image_error}"
            )));
        }
        Ok(())
    }

    pub async fn graceful_stop(
        &mut self,
        timeout: Duration,
    ) -> Result<(), FirecrackerLifecycleError> {
        let process_result = self.process.graceful_stop(timeout).await;
        let image_result = self.image.remove();
        if let Err(process_error) = process_result {
            let image_error = image_result.err();
            return Err(stop_error(process_error, image_error));
        }
        if let Err(image_error) = image_result {
            return Err(FirecrackerLifecycleError::new(format!(
                "firecracker image cleanup failed: {image_error}"
            )));
        }
        Ok(())
    }

    pub fn api_socket_path(&self) -> &Path {
        self.process.api_socket_path()
    }

    pub fn config_file_path(&self) -> &Path {
        self.process.config_file_path()
    }

    pub fn rootfs_path(&self) -> &Path {
        self.image.path()
    }
}

fn validate_host_path(label: &'static str, path: &Path) -> Result<(), FirecrackerLifecycleError> {
    if path.as_os_str().is_empty() {
        return Err(FirecrackerLifecycleError::new(format!(
            "{label} path is empty"
        )));
    }
    if !path.is_absolute() {
        return Err(FirecrackerLifecycleError::new(format!(
            "{label} path must be absolute"
        )));
    }
    if path_contains_nul(path) {
        return Err(FirecrackerLifecycleError::new(format!(
            "{label} path contains a NUL byte"
        )));
    }
    if contains_dot_component(path) {
        return Err(FirecrackerLifecycleError::new(format!(
            "{label} path must not contain . or .. components"
        )));
    }
    Ok(())
}

fn contextual_error(label: &str, error: FirecrackerImageError) -> FirecrackerLifecycleError {
    FirecrackerLifecycleError::new(format!("invalid {label}: {error}"))
}

fn startup_error(
    error: impl Display,
    stop_error: Option<FirecrackerProcessError>,
    image_error: Option<FirecrackerImageError>,
) -> FirecrackerLifecycleError {
    let mut message = format!("firecracker lifecycle startup failed: {error}");
    if let Some(stop_error) = stop_error {
        message.push_str(&format!("; process cleanup failed: {stop_error}"));
    }
    if let Some(image_error) = image_error {
        message.push_str(&format!("; image cleanup failed: {image_error}"));
    }
    FirecrackerLifecycleError::new(message)
}

fn stop_error(
    process_error: FirecrackerProcessError,
    image_error: Option<FirecrackerImageError>,
) -> FirecrackerLifecycleError {
    let mut message = format!("firecracker process cleanup failed: {process_error}");
    if let Some(image_error) = image_error {
        message.push_str(&format!("; image cleanup failed: {image_error}"));
    }
    FirecrackerLifecycleError::new(message)
}

fn path_contains_nul(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().contains(&0)
}

fn contains_dot_component(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str()
        .as_bytes()
        .split(|byte| *byte == b'/')
        .any(|component| component == b"." || component == b"..")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_regular_file(path: &Path, contents: &[u8]) {
        std::fs::write(path, contents).expect("write regular file");
    }

    fn set_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)
            .expect("file metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).expect("set executable permissions");
    }

    fn test_environment() -> (tempfile::TempDir, FirecrackerLifecycleConfig) {
        let root = tempfile::tempdir().expect("temp directory");
        let binary_path = root.path().join("firecracker");
        let kernel_path = root.path().join("vmlinux");
        let base_image_path = root.path().join("base.ext4");
        let run_directory = root.path().join("run");
        write_regular_file(&binary_path, b"#!/bin/sh\nexec sleep 30\n");
        set_executable(&binary_path);
        write_regular_file(&kernel_path, b"kernel");
        write_regular_file(&base_image_path, b"rootfs");
        std::fs::create_dir(&run_directory).expect("create run directory");
        let config = FirecrackerLifecycleConfig::new(
            &binary_path,
            &kernel_path,
            &base_image_path,
            &run_directory,
            1,
            128,
            3,
        );
        (root, config)
    }

    #[test]
    fn config_derives_distinct_artifact_paths() {
        let (root, config) = test_environment();
        config.validate().expect("valid lifecycle config");
        assert_eq!(config.rootfs_path(), root.path().join("run/rootfs.ext4"));
        assert_eq!(
            config.api_socket_path(),
            root.path().join("run/firecracker.sock")
        );
        assert_eq!(
            config.config_file_path(),
            root.path().join("run/firecracker.json")
        );
        assert_eq!(
            config.vsock_socket_path(),
            root.path().join("run/guest-agent.sock")
        );
    }

    #[test]
    fn config_rejects_existing_artifacts() {
        let (_root, config) = test_environment();
        let config = config.with_readiness_timeout(Duration::from_millis(1));
        std::fs::write(config.rootfs_path(), b"existing").expect("write existing rootfs");
        let error = config.validate().expect_err("rootfs already exists");
        assert!(error.to_string().contains("rootfs path already exists"));
    }

    #[test]
    fn config_rejects_input_artifact_collision() {
        let (root, config) = test_environment();
        let config = FirecrackerLifecycleConfig::new(
            config.binary_path.clone(),
            config.kernel_image_path.clone(),
            config.rootfs_path(),
            root.path().join("run"),
            1,
            128,
            3,
        );
        let error = config
            .validate()
            .expect_err("base image collides with rootfs");
        assert!(
            error
                .to_string()
                .contains("base image path must not be the rootfs path")
        );
    }

    #[test]
    fn config_rejects_non_executable_binary() {
        let (root, config) = test_environment();
        let permissions = std::fs::metadata(root.path().join("firecracker"))
            .expect("binary metadata")
            .permissions();
        let mut permissions = permissions;
        permissions.set_mode(0o600);
        std::fs::set_permissions(root.path().join("firecracker"), permissions)
            .expect("remove executable permission");
        let error = config.validate().expect_err("binary is not executable");
        assert!(error.to_string().contains("binary must be executable"));
    }

    #[test]
    fn config_rejects_run_directory_symlink() {
        let (root, config) = test_environment();
        let actual_run_directory = tempfile::tempdir().expect("actual run directory");
        let run_directory_symlink = root.path().join("run-symlink");
        std::os::unix::fs::symlink(actual_run_directory.path(), &run_directory_symlink)
            .expect("create run directory symlink");
        let config = FirecrackerLifecycleConfig::new(
            config.binary_path.clone(),
            config.kernel_image_path.clone(),
            config.base_image_path.clone(),
            run_directory_symlink,
            1,
            128,
            3,
        );

        let error = config.validate().expect_err("run directory is a symlink");
        assert!(error.to_string().contains("invalid run directory"));
    }

    #[tokio::test]
    async fn start_and_stop_clean_lifecycle_artifacts() {
        let (root, config) = test_environment();
        let binary_path = root.path().join("firecracker");
        write_regular_file(
            &binary_path,
            b"#!/usr/bin/env python3\nimport socket, sys\nserver = socket.socket(socket.AF_UNIX)\nserver.bind(sys.argv[2])\nserver.listen(4)\nwhile True:\n    connection, _ = server.accept()\n    request = connection.recv(4096)\n    if request.startswith(b\"PUT /actions\"):\n        connection.sendall(b\"HTTP/1.1 204 No Content\\r\\nContent-Length: 0\\r\\nConnection: close\\r\\n\\r\\n\")\n        connection.close()\n        break\n    connection.close()\n",
        );
        set_executable(&binary_path);
        let mut lifecycle = FirecrackerLifecycle::start(&config)
            .await
            .expect("start lifecycle");

        assert!(config.rootfs_path().is_file());
        assert!(config.api_socket_path().exists());
        assert!(config.config_file_path().is_file());
        assert_eq!(lifecycle.rootfs_path(), config.rootfs_path());
        lifecycle
            .graceful_stop(Duration::from_secs(2))
            .await
            .expect("graceful stop lifecycle");
        lifecycle
            .graceful_stop(Duration::from_secs(2))
            .await
            .expect("graceful stop is idempotent");
        assert!(!config.rootfs_path().exists());
        assert!(!config.api_socket_path().exists());
        assert!(!config.config_file_path().exists());
    }

    #[tokio::test]
    async fn startup_failure_cleans_lifecycle_artifacts() {
        let (root, config) = test_environment();
        let binary_path = root.path().join("firecracker");
        write_regular_file(&binary_path, b"#!/bin/sh\nexit 7\n");
        set_executable(&binary_path);
        let config = config.with_readiness_timeout(Duration::from_millis(500));

        let error = match FirecrackerLifecycle::start(&config).await {
            Ok(_) => panic!("fake firecracker unexpectedly started"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("firecracker process exited before ready")
        );
        assert!(!config.rootfs_path().exists());
        assert!(!config.api_socket_path().exists());
        assert!(!config.config_file_path().exists());
    }

    #[tokio::test]
    async fn drop_cleans_lifecycle_artifacts() {
        let (root, config) = test_environment();
        let binary_path = root.path().join("firecracker");
        write_regular_file(
            &binary_path,
            b"#!/usr/bin/env python3\nimport socket, sys, time\nserver = socket.socket(socket.AF_UNIX)\nserver.bind(sys.argv[2])\nserver.listen(1)\ntime.sleep(30)\n",
        );
        set_executable(&binary_path);
        let lifecycle = FirecrackerLifecycle::start(&config)
            .await
            .expect("start lifecycle");

        drop(lifecycle);
        assert!(!config.rootfs_path().exists());
        assert!(!config.api_socket_path().exists());
        assert!(!config.config_file_path().exists());
    }
}
