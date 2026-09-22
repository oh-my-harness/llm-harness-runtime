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
use crate::jail::{FirecrackerJail, FirecrackerJailError};
use crate::jailer::{FirecrackerJailerConfig, FirecrackerJailerConfigError};
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

impl From<FirecrackerJailerConfigError> for FirecrackerLifecycleError {
    fn from(error: FirecrackerJailerConfigError) -> Self {
        Self::new(error.to_string())
    }
}

impl From<FirecrackerJailError> for FirecrackerLifecycleError {
    fn from(error: FirecrackerJailError) -> Self {
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
    jailer: Option<FirecrackerJailerConfig>,
}

#[derive(Debug, Clone)]
struct FirecrackerLifecyclePaths {
    kernel_image_path: PathBuf,
    rootfs_path: PathBuf,
    api_socket_path: PathBuf,
    config_file_path: PathBuf,
    vsock_socket_path: PathBuf,
    chroot_dir: Option<PathBuf>,
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
            jailer: None,
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

    pub fn with_jailer(mut self, jailer: FirecrackerJailerConfig) -> Self {
        self.jailer = Some(jailer);
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
        let paths = self.lifecycle_paths()?;
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
            ("rootfs", &paths.rootfs_path),
            ("API socket", &paths.api_socket_path),
            ("config file", &paths.config_file_path),
            ("vsock socket", &paths.vsock_socket_path),
        ];
        for (input_label, input_path) in inputs {
            for (artifact_label, artifact_path) in &artifacts {
                if input_path == *artifact_path {
                    return Err(FirecrackerLifecycleError::new(format!(
                        "{input_label} path must not be the {artifact_label} path"
                    )));
                }
            }
        }
        if let Some(chroot_dir) = &paths.chroot_dir {
            if self.run_directory.starts_with(chroot_dir) {
                return Err(FirecrackerLifecycleError::new(
                    "run directory must be outside the jailer chroot",
                ));
            }
            if let Some(jailer) = &self.jailer
                && jailer.firecracker_binary_path() != self.binary_path
            {
                return Err(FirecrackerLifecycleError::new(
                    "lifecycle binary path must match the jailer Firecracker binary path",
                ));
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
        let paths = self.lifecycle_paths()?;
        open_directory(&self.run_directory).map_err(|error| {
            FirecrackerLifecycleError::new(format!("invalid run directory: {error}"))
        })?;
        self.process_config()?.validate()?;
        self.vm_config(paths.kernel_image_path, paths.rootfs_path)?
            .validate()?;
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
        if let Some(jailer) = &self.jailer {
            let jailer_binary = open_regular_file(jailer.jailer_binary_path())
                .map_err(|error| contextual_error("jailer binary", error))?;
            let jailer_metadata = jailer_binary.metadata()?;
            if jailer_metadata.len() == 0 {
                return Err(FirecrackerLifecycleError::new(
                    "jailer binary must not be empty",
                ));
            }
            if jailer_metadata.permissions().mode() & 0o111 == 0 {
                return Err(FirecrackerLifecycleError::new(
                    "jailer binary must be executable",
                ));
            }
        }
        Ok(())
    }

    fn validate_artifact_paths_are_absent(&self) -> Result<(), FirecrackerLifecycleError> {
        let paths = self.lifecycle_paths()?;
        let mut artifacts = vec![
            ("rootfs", paths.rootfs_path),
            ("API socket", paths.api_socket_path),
            ("config file", paths.config_file_path),
            ("vsock socket", paths.vsock_socket_path),
        ];
        if let Some(chroot_dir) = paths.chroot_dir {
            artifacts.push(("jail root", chroot_dir));
        }
        for (label, path) in artifacts {
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

    fn process_config(&self) -> Result<FirecrackerProcessConfig, FirecrackerLifecycleError> {
        let paths = self.lifecycle_paths()?;
        Ok(FirecrackerProcessConfig::new(
            self.binary_path.clone(),
            paths.api_socket_path,
            paths.config_file_path,
        ))
    }

    fn vm_config(
        &self,
        kernel_image_path: PathBuf,
        rootfs_path: PathBuf,
    ) -> Result<FirecrackerConfig, FirecrackerLifecycleError> {
        let paths = self.lifecycle_paths()?;
        Ok(FirecrackerConfig::new(
            kernel_image_path,
            rootfs_path,
            paths.vsock_socket_path,
            self.vcpu_count,
            self.mem_size_mib,
            self.vsock_guest_cid,
        )
        .with_boot_args(self.boot_args.clone())
        .with_rootfs_read_only(self.rootfs_read_only))
    }

    fn lifecycle_paths(&self) -> Result<FirecrackerLifecyclePaths, FirecrackerLifecycleError> {
        if let Some(jailer) = &self.jailer {
            let chroot_dir = jailer.chroot_dir()?;
            return Ok(FirecrackerLifecyclePaths {
                kernel_image_path: chroot_dir.join("vmlinux"),
                rootfs_path: chroot_dir.join(ROOTFS_FILE_NAME),
                api_socket_path: chroot_dir.join(API_SOCKET_FILE_NAME),
                config_file_path: chroot_dir.join(CONFIG_FILE_NAME),
                vsock_socket_path: chroot_dir.join(VSOCK_SOCKET_FILE_NAME),
                chroot_dir: Some(chroot_dir),
            });
        }
        Ok(FirecrackerLifecyclePaths {
            kernel_image_path: self.kernel_image_path.clone(),
            rootfs_path: self.run_directory.join(ROOTFS_FILE_NAME),
            api_socket_path: self.run_directory.join(API_SOCKET_FILE_NAME),
            config_file_path: self.run_directory.join(CONFIG_FILE_NAME),
            vsock_socket_path: self.run_directory.join(VSOCK_SOCKET_FILE_NAME),
            chroot_dir: None,
        })
    }
}

pub struct FirecrackerLifecycle {
    process: FirecrackerProcess,
    image: FirecrackerImage,
    jail: Option<FirecrackerJail>,
}

impl FirecrackerLifecycle {
    pub async fn start(
        config: &FirecrackerLifecycleConfig,
    ) -> Result<Self, FirecrackerLifecycleError> {
        config.validate()?;
        let paths = config.lifecycle_paths()?;
        let mut jail = None;
        if let Some(jailer_config) = &config.jailer {
            let chroot_dir = paths
                .chroot_dir
                .clone()
                .ok_or_else(|| FirecrackerLifecycleError::new("jail root is missing"))?;
            let mut created_jail = FirecrackerJail::create(&chroot_dir)?;
            if let Err(error) = created_jail.prepare(jailer_config.uid(), jailer_config.gid()) {
                let jail_error = created_jail.remove().err();
                return Err(startup_error(error, None, None, jail_error));
            }
            if let Err(error) = created_jail.stage_kernel(
                &config.kernel_image_path,
                jailer_config.uid(),
                jailer_config.gid(),
            ) {
                let jail_error = created_jail.remove().err();
                return Err(startup_error(error, None, None, jail_error));
            }
            jail = Some(created_jail);
        }

        let mut image = FirecrackerImageAllocator::new(&config.base_image_path)
            .allocate(paths.rootfs_path.clone())?;
        if let Some(created_jail) = jail.as_mut()
            && let Some(jailer_config) = &config.jailer
            && let Err(error) =
                created_jail.stage_rootfs(image.path(), jailer_config.uid(), jailer_config.gid())
        {
            let image_error = image.remove().err();
            let jail_error = cleanup_jail(&mut jail);
            return Err(startup_error(error, None, image_error, jail_error));
        }
        let vm_config =
            config.vm_config(paths.kernel_image_path.clone(), image.path().to_path_buf())?;
        let process_config = config.process_config()?;
        let start_result = if let Some(jailer_config) = &config.jailer {
            FirecrackerProcess::start_jailed(jailer_config, &process_config, &vm_config)
        } else {
            FirecrackerProcess::start(&process_config, &vm_config)
        };
        let mut process = match start_result {
            Ok(process) => process,
            Err(error) => {
                let image_error = image.remove().err();
                let jail_error = cleanup_jail(&mut jail);
                return Err(startup_error(error, None, image_error, jail_error));
            }
        };

        if let Err(error) = process.wait_ready(config.readiness_timeout).await {
            let stop_error = process.stop().await.err();
            let image_error = image.remove().err();
            let jail_error = cleanup_jail(&mut jail);
            return Err(startup_error(error, stop_error, image_error, jail_error));
        }

        Ok(Self {
            process,
            image,
            jail,
        })
    }

    pub async fn stop(&mut self) -> Result<(), FirecrackerLifecycleError> {
        let process_result = self.process.stop().await;
        let image_result = self.image.remove();
        let jail_result = cleanup_jail(&mut self.jail);
        if let Err(process_error) = process_result {
            let image_error = image_result.err();
            let jail_error = jail_result;
            return Err(stop_error(process_error, image_error, jail_error));
        }
        if let Err(image_error) = image_result {
            let jail_error = jail_result;
            return Err(stop_cleanup_error(image_error.to_string(), jail_error));
        }
        if let Some(jail_error) = jail_result {
            return Err(FirecrackerLifecycleError::new(format!(
                "firecracker jail cleanup failed: {jail_error}"
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
        let jail_result = cleanup_jail(&mut self.jail);
        if let Err(process_error) = process_result {
            let image_error = image_result.err();
            let jail_error = jail_result;
            return Err(stop_error(process_error, image_error, jail_error));
        }
        if let Err(image_error) = image_result {
            let jail_error = jail_result;
            return Err(stop_cleanup_error(image_error.to_string(), jail_error));
        }
        if let Some(jail_error) = jail_result {
            return Err(FirecrackerLifecycleError::new(format!(
                "firecracker jail cleanup failed: {jail_error}"
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
    jail_error: Option<FirecrackerJailError>,
) -> FirecrackerLifecycleError {
    let mut message = format!("firecracker lifecycle startup failed: {error}");
    if let Some(stop_error) = stop_error {
        message.push_str(&format!("; process cleanup failed: {stop_error}"));
    }
    if let Some(image_error) = image_error {
        message.push_str(&format!("; image cleanup failed: {image_error}"));
    }
    if let Some(jail_error) = jail_error {
        message.push_str(&format!("; jail cleanup failed: {jail_error}"));
    }
    FirecrackerLifecycleError::new(message)
}

fn stop_error(
    process_error: FirecrackerProcessError,
    image_error: Option<FirecrackerImageError>,
    jail_error: Option<FirecrackerJailError>,
) -> FirecrackerLifecycleError {
    let mut message = format!("firecracker process cleanup failed: {process_error}");
    if let Some(image_error) = image_error {
        message.push_str(&format!("; image cleanup failed: {image_error}"));
    }
    if let Some(jail_error) = jail_error {
        message.push_str(&format!("; jail cleanup failed: {jail_error}"));
    }
    FirecrackerLifecycleError::new(message)
}

fn stop_cleanup_error(
    message: String,
    jail_error: Option<FirecrackerJailError>,
) -> FirecrackerLifecycleError {
    let mut message = format!("firecracker lifecycle cleanup failed: {message}");
    if let Some(jail_error) = jail_error {
        message.push_str(&format!("; jail cleanup failed: {jail_error}"));
    }
    FirecrackerLifecycleError::new(message)
}

fn cleanup_jail(jail: &mut Option<FirecrackerJail>) -> Option<FirecrackerJailError> {
    jail.as_mut().and_then(|jail| jail.remove().err())
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

    #[cfg(target_os = "linux")]
    fn non_root_ids() -> (u32, u32) {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        if uid == 0 || gid == 0 {
            (1000, 1000)
        } else {
            (uid, gid)
        }
    }

    #[cfg(target_os = "linux")]
    fn jailed_lifecycle_environment() -> (
        tempfile::TempDir,
        FirecrackerLifecycleConfig,
        FirecrackerJailerConfig,
        PathBuf,
    ) {
        let root = tempfile::tempdir().expect("temp directory");
        let firecracker_binary_path = root.path().join("firecracker");
        let jailer_binary_path = root.path().join("fake-jailer");
        let kernel_path = root.path().join("vmlinux");
        let base_image_path = root.path().join("base.ext4");
        let run_directory = root.path().join("run");
        let chroot_base_dir = root.path().join("jail");
        let chroot_dir = chroot_base_dir
            .join("firecracker")
            .join("sandbox-1")
            .join("root");

        write_regular_file(&firecracker_binary_path, b"fake firecracker");
        set_executable(&firecracker_binary_path);
        write_regular_file(&kernel_path, b"kernel");
        write_regular_file(&base_image_path, b"rootfs");
        std::fs::create_dir(&run_directory).expect("create run directory");

        let (uid, gid) = non_root_ids();
        let jailer_script = format!(
            r#"#!/usr/bin/env python3
import json, os, socket, sys, time
args = sys.argv[1:]
api_path = args[args.index("--api-sock") + 1]
config_path = args[args.index("--config-file") + 1]
chroot = {:?}
with open(chroot + config_path) as config_file:
    config = json.load(config_file)
assert config["boot-source"]["kernel_image_path"] == "/vmlinux"
assert config["drives"][0]["path_on_host"] == "/rootfs.ext4"
assert config["vsock"]["uds_path"] == "/guest-agent.sock"
os.makedirs(chroot + "/nested/deep")
with open(chroot + "/nested/deep/child", "w") as child:
    child.write("child")
os.symlink("/etc/passwd", chroot + "/nested/link")
server = socket.socket(socket.AF_UNIX)
server.bind(chroot + api_path)
server.listen(1)
time.sleep(30)
"#,
            chroot_dir
        );
        write_regular_file(&jailer_binary_path, jailer_script.as_bytes());
        set_executable(&jailer_binary_path);

        let jailer = FirecrackerJailerConfig::new(
            &jailer_binary_path,
            &firecracker_binary_path,
            "sandbox-1",
            uid,
            gid,
        )
        .with_chroot_base_dir(&chroot_base_dir);
        let config = FirecrackerLifecycleConfig::new(
            &firecracker_binary_path,
            &kernel_path,
            &base_image_path,
            &run_directory,
            1,
            128,
            3,
        )
        .with_readiness_timeout(Duration::from_millis(1000))
        .with_jailer(jailer.clone());
        (root, config, jailer, chroot_dir)
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

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn jailed_lifecycle_stages_resources_and_cleans_the_jail() {
        use std::os::unix::fs::MetadataExt;

        let (root, config, jailer, chroot_dir) = jailed_lifecycle_environment();
        let mut lifecycle = FirecrackerLifecycle::start(&config)
            .await
            .expect("start jailed lifecycle");

        assert_eq!(lifecycle.rootfs_path(), chroot_dir.join("rootfs.ext4"));
        assert_eq!(
            lifecycle.api_socket_path(),
            chroot_dir.join("firecracker.sock")
        );
        let config_json: serde_json::Value = serde_json::from_slice(
            &std::fs::read(chroot_dir.join("firecracker.json")).expect("read jailed config"),
        )
        .expect("parse jailed config");
        assert_eq!(config_json["boot-source"]["kernel_image_path"], "/vmlinux");
        assert_eq!(config_json["drives"][0]["path_on_host"], "/rootfs.ext4");
        assert_eq!(config_json["vsock"]["uds_path"], "/guest-agent.sock");

        for resource in ["vmlinux", "rootfs.ext4", "firecracker.json"] {
            let metadata =
                std::fs::metadata(chroot_dir.join(resource)).expect("jailed resource metadata");
            assert_eq!(metadata.uid(), jailer.uid());
            assert_eq!(metadata.gid(), jailer.gid());
        }

        lifecycle.stop().await.expect("stop jailed lifecycle");
        assert!(!chroot_dir.exists());
        assert!(!root.path().join("jail").exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn jailed_lifecycle_startup_failure_cleans_the_jail() {
        let (root, config, _jailer, chroot_dir) = jailed_lifecycle_environment();
        write_regular_file(&root.path().join("fake-jailer"), b"#!/bin/sh\nexit 7\n");
        set_executable(&root.path().join("fake-jailer"));

        let error = match FirecrackerLifecycle::start(&config).await {
            Ok(_) => panic!("fake jailer unexpectedly stayed ready"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("firecracker process exited before ready")
        );
        assert!(!chroot_dir.exists());
        assert!(!root.path().join("jail").exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn jailed_lifecycle_rejects_an_existing_chroot() {
        let (_root, config, _jailer, chroot_dir) = jailed_lifecycle_environment();
        std::fs::create_dir_all(&chroot_dir).expect("create existing chroot");

        let error = config.validate().expect_err("jail root already exists");
        assert!(error.to_string().contains("jail root path already exists"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn jailed_lifecycle_rejects_chroot_parent_symlink() {
        let (root, config, _jailer, chroot_dir) = jailed_lifecycle_environment();
        let actual_directory = tempfile::tempdir().expect("actual directory");
        std::fs::create_dir(root.path().join("jail")).expect("create chroot base");
        std::os::unix::fs::symlink(
            actual_directory.path(),
            root.path().join("jail/firecracker"),
        )
        .expect("create chroot parent symlink");

        let error = match FirecrackerLifecycle::start(&config).await {
            Ok(_) => panic!("jailed lifecycle unexpectedly accepted a symlink"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("firecracker jail io error"));
        assert!(!chroot_dir.exists());
    }
}
