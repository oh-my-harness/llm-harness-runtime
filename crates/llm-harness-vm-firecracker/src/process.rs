use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

#[cfg(target_os = "linux")]
use tokio::net::UnixStream;
#[cfg(target_os = "linux")]
use tokio::process::Child;

#[cfg(target_os = "linux")]
use crate::config::FirecrackerConfig;
use crate::config::FirecrackerConfigError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirecrackerProcessConfig {
    binary_path: PathBuf,
    api_socket_path: PathBuf,
    config_file_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct FirecrackerProcessConfigError {
    message: String,
}

impl FirecrackerProcessConfig {
    pub fn new(
        binary_path: impl Into<PathBuf>,
        api_socket_path: impl Into<PathBuf>,
        config_file_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            binary_path: binary_path.into(),
            api_socket_path: api_socket_path.into(),
            config_file_path: config_file_path.into(),
        }
    }

    pub fn validate(&self) -> Result<(), FirecrackerProcessConfigError> {
        validate_absolute_path("binary", &self.binary_path)?;
        validate_absolute_path("API socket", &self.api_socket_path)?;
        validate_absolute_path("config file", &self.config_file_path)?;
        if self.api_socket_path == self.config_file_path {
            return Err(FirecrackerProcessConfigError {
                message: "API socket and config file paths must be different".to_owned(),
            });
        }
        Ok(())
    }

    pub fn command_args(&self) -> Vec<OsString> {
        vec![
            OsString::from("--api-sock"),
            self.api_socket_path.as_os_str().to_owned(),
            OsString::from("--config-file"),
            self.config_file_path.as_os_str().to_owned(),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct FirecrackerProcessError {
    message: String,
}

impl From<FirecrackerProcessConfigError> for FirecrackerProcessError {
    fn from(error: FirecrackerProcessConfigError) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

impl From<FirecrackerConfigError> for FirecrackerProcessError {
    fn from(error: FirecrackerConfigError) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

impl From<std::io::Error> for FirecrackerProcessError {
    fn from(error: std::io::Error) -> Self {
        Self {
            message: format!("firecracker process io error: {error}"),
        }
    }
}

impl From<serde_json::Error> for FirecrackerProcessError {
    fn from(error: serde_json::Error) -> Self {
        Self {
            message: format!("firecracker config json error: {error}"),
        }
    }
}

#[cfg(target_os = "linux")]
pub struct FirecrackerProcess {
    child: Child,
    api_socket_path: PathBuf,
    config_file_path: PathBuf,
    stopped: bool,
}

#[cfg(target_os = "linux")]
impl FirecrackerProcess {
    pub fn start(
        config: &FirecrackerProcessConfig,
        vm_config: &FirecrackerConfig,
    ) -> Result<Self, FirecrackerProcessError> {
        config.validate()?;
        vm_config.validate()?;
        let config_json = serde_json::to_vec_pretty(&vm_config.to_json()?)?;
        if let Some(parent) = config.config_file_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Some(parent) = config.api_socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::symlink_metadata(&config.api_socket_path) {
            Ok(_) => {
                return Err(FirecrackerProcessError {
                    message: "API socket path already exists".to_owned(),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        write_config_file(&config.config_file_path, &config_json)?;

        let mut command = tokio::process::Command::new(&config.binary_path);
        command.args(config.command_args());
        command.kill_on_drop(true);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = std::fs::remove_file(&config.config_file_path);
                return Err(error.into());
            }
        };
        Ok(Self {
            child,
            api_socket_path: config.api_socket_path.clone(),
            config_file_path: config.config_file_path.clone(),
            stopped: false,
        })
    }

    pub async fn stop(&mut self) -> Result<(), FirecrackerProcessError> {
        if self.stopped {
            return Ok(());
        }
        let _ = self.child.start_kill();
        let wait_result = self.child.wait().await;
        let cleanup_result = remove_artifacts(&self.api_socket_path, &self.config_file_path);
        if wait_result.is_ok() && cleanup_result.is_ok() {
            self.stopped = true;
        }
        wait_result?;
        cleanup_result?;
        Ok(())
    }

    pub async fn wait_ready(&mut self, timeout: Duration) -> Result<(), FirecrackerProcessError> {
        if self.stopped {
            return Err(FirecrackerProcessError {
                message: "firecracker process is stopped".to_owned(),
            });
        }
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Err(FirecrackerProcessError {
                    message: format!("firecracker process exited before ready: {status}"),
                });
            }
            if UnixStream::connect(&self.api_socket_path).await.is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(FirecrackerProcessError {
                    message: format!("firecracker process did not become ready within {timeout:?}"),
                });
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub fn api_socket_path(&self) -> &Path {
        &self.api_socket_path
    }

    pub fn config_file_path(&self) -> &Path {
        &self.config_file_path
    }
}

#[cfg(target_os = "linux")]
impl Drop for FirecrackerProcess {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        let _ = self.child.start_kill();
        let _ = remove_artifacts(&self.api_socket_path, &self.config_file_path);
    }
}

#[cfg(target_os = "linux")]
fn write_config_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    if let Err(error) = file.write_all(contents) {
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    if let Err(error) = file.sync_all() {
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn remove_artifacts(api_socket_path: &Path, config_file_path: &Path) -> std::io::Result<()> {
    let api_socket_error = remove_owned_file(api_socket_path).err();
    let config_file_error = remove_owned_file(config_file_path).err();
    if let Some(error) = api_socket_error.or(config_file_error) {
        return Err(error);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn remove_owned_file(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn validate_absolute_path(
    label: &'static str,
    path: &Path,
) -> Result<(), FirecrackerProcessConfigError> {
    if path.as_os_str().is_empty() {
        return Err(FirecrackerProcessConfigError {
            message: format!("{label} path is empty"),
        });
    }
    if !path.is_absolute() {
        return Err(FirecrackerProcessConfigError {
            message: format!("{label} path must be absolute"),
        });
    }
    if path_contains_nul(path) {
        return Err(FirecrackerProcessConfigError {
            message: format!("{label} path contains a NUL byte"),
        });
    }
    Ok(())
}

fn path_contains_nul(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        path.as_os_str().as_bytes().contains(&0)
    }
    #[cfg(not(unix))]
    {
        path.to_str().is_some_and(|path| path.contains('\0'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_config_builds_firecracker_command_args() {
        let config = FirecrackerProcessConfig::new(
            std::env::temp_dir().join("firecracker"),
            std::env::temp_dir().join("llm-harness-api.sock"),
            std::env::temp_dir().join("llm-harness-firecracker.json"),
        );
        config.validate().expect("valid process config");
        assert_eq!(
            config.command_args(),
            vec![
                OsString::from("--api-sock"),
                std::env::temp_dir()
                    .join("llm-harness-api.sock")
                    .as_os_str()
                    .to_owned(),
                OsString::from("--config-file"),
                std::env::temp_dir()
                    .join("llm-harness-firecracker.json")
                    .as_os_str()
                    .to_owned(),
            ]
        );
    }

    #[test]
    fn process_config_rejects_relative_paths() {
        let config = FirecrackerProcessConfig::new(
            PathBuf::from("firecracker"),
            std::env::temp_dir().join("llm-harness-api.sock"),
            std::env::temp_dir().join("llm-harness-firecracker.json"),
        );
        assert!(config.validate().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn process_config_preserves_non_utf8_paths() {
        use std::os::unix::ffi::OsStringExt;

        let api_socket_path =
            PathBuf::from(OsString::from_vec(b"/tmp/llm-harness-\xff.sock".to_vec()));
        let config_file_path =
            PathBuf::from(OsString::from_vec(b"/tmp/llm-harness-\xff.json".to_vec()));
        let config = FirecrackerProcessConfig::new(
            "/usr/bin/firecracker",
            api_socket_path.clone(),
            config_file_path.clone(),
        );

        assert_eq!(
            config.command_args(),
            vec![
                OsString::from("--api-sock"),
                api_socket_path.as_os_str().to_owned(),
                OsString::from("--config-file"),
                config_file_path.as_os_str().to_owned(),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn process_config_rejects_nul_paths() {
        use std::os::unix::ffi::OsStringExt;

        let api_socket_path =
            PathBuf::from(OsString::from_vec(b"/tmp/llm-harness-\0.sock".to_vec()));
        let config_file_path = PathBuf::from(OsString::from_vec(b"/tmp/llm-harness.json".to_vec()));
        let config = FirecrackerProcessConfig::new(
            "/usr/bin/firecracker",
            api_socket_path,
            config_file_path,
        );

        assert!(config.validate().is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn start_failure_removes_the_config_file() {
        let root = tempfile::tempdir().expect("temp directory");
        let vm_config = FirecrackerConfig::new(
            root.path().join("vmlinux"),
            root.path().join("rootfs.ext4"),
            root.path().join("guest-agent.sock"),
            1,
            128,
            3,
        );
        let process_config = FirecrackerProcessConfig::new(
            root.path().join("missing-firecracker"),
            root.path().join("api.sock"),
            root.path().join("firecracker.json"),
        );

        let error = match FirecrackerProcess::start(&process_config, &vm_config) {
            Err(error) => error,
            Ok(_) => panic!("missing binary unexpectedly started"),
        };
        assert!(error.to_string().contains("firecracker process io error"));
        assert!(!process_config.config_file_path.exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn stop_kills_the_process_and_removes_artifacts() {
        let root = tempfile::tempdir().expect("temp directory");
        let binary_path = root.path().join("fake-firecracker");
        std::fs::write(
            &binary_path,
            "#!/bin/sh\napi_socket=$2\n: > \"$api_socket\"\nexec sleep 30\n",
        )
        .expect("write fake firecracker");
        set_executable(&binary_path);
        let vm_config = FirecrackerConfig::new(
            root.path().join("vmlinux"),
            root.path().join("rootfs.ext4"),
            root.path().join("guest-agent.sock"),
            1,
            128,
            3,
        );
        let process_config = FirecrackerProcessConfig::new(
            &binary_path,
            root.path().join("api.sock"),
            root.path().join("firecracker.json"),
        );
        let mut process =
            FirecrackerProcess::start(&process_config, &vm_config).expect("start fake firecracker");

        wait_for_file(&process_config.api_socket_path).await;
        assert!(process_config.config_file_path.exists());
        process.stop().await.expect("stop fake firecracker");
        assert!(!process_config.api_socket_path.exists());
        assert!(!process_config.config_file_path.exists());
        process.stop().await.expect("stop is idempotent");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn drop_removes_artifacts_without_explicit_stop() {
        let root = tempfile::tempdir().expect("temp directory");
        let binary_path = root.path().join("fake-firecracker");
        std::fs::write(
            &binary_path,
            "#!/bin/sh\napi_socket=$2\n: > \"$api_socket\"\nexec sleep 30\n",
        )
        .expect("write fake firecracker");
        set_executable(&binary_path);
        let vm_config = FirecrackerConfig::new(
            root.path().join("vmlinux"),
            root.path().join("rootfs.ext4"),
            root.path().join("guest-agent.sock"),
            1,
            128,
            3,
        );
        let process_config = FirecrackerProcessConfig::new(
            &binary_path,
            root.path().join("api.sock"),
            root.path().join("firecracker.json"),
        );
        let process =
            FirecrackerProcess::start(&process_config, &vm_config).expect("start fake firecracker");

        wait_for_file(&process_config.api_socket_path).await;
        drop(process);
        assert!(!process_config.api_socket_path.exists());
        assert!(!process_config.config_file_path.exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn wait_ready_succeeds_when_api_socket_is_listening() {
        use std::os::unix::net::UnixListener;

        let root = tempfile::tempdir().expect("temp directory");
        let binary_path = root.path().join("fake-firecracker");
        std::fs::write(&binary_path, "#!/bin/sh\nexec sleep 30\n").expect("write fake firecracker");
        set_executable(&binary_path);
        let vm_config = FirecrackerConfig::new(
            root.path().join("vmlinux"),
            root.path().join("rootfs.ext4"),
            root.path().join("guest-agent.sock"),
            1,
            128,
            3,
        );
        let process_config = FirecrackerProcessConfig::new(
            &binary_path,
            root.path().join("api.sock"),
            root.path().join("firecracker.json"),
        );
        let mut process =
            FirecrackerProcess::start(&process_config, &vm_config).expect("start fake firecracker");
        let listener =
            UnixListener::bind(&process_config.api_socket_path).expect("bind fake api socket");

        process
            .wait_ready(Duration::from_millis(500))
            .await
            .expect("firecracker becomes ready");
        process.stop().await.expect("stop fake firecracker");
        drop(listener);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn wait_ready_fails_when_process_exits() {
        let root = tempfile::tempdir().expect("temp directory");
        let binary_path = root.path().join("fake-firecracker");
        std::fs::write(&binary_path, "#!/bin/sh\nexit 7\n").expect("write fake firecracker");
        set_executable(&binary_path);
        let vm_config = FirecrackerConfig::new(
            root.path().join("vmlinux"),
            root.path().join("rootfs.ext4"),
            root.path().join("guest-agent.sock"),
            1,
            128,
            3,
        );
        let process_config = FirecrackerProcessConfig::new(
            &binary_path,
            root.path().join("api.sock"),
            root.path().join("firecracker.json"),
        );
        let mut process =
            FirecrackerProcess::start(&process_config, &vm_config).expect("start fake firecracker");

        let error = process
            .wait_ready(Duration::from_millis(500))
            .await
            .expect_err("firecracker exits before ready");
        assert!(error.to_string().contains("exited before ready"));
        process.stop().await.expect("stop fake firecracker");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn wait_ready_times_out_when_api_socket_is_missing() {
        let root = tempfile::tempdir().expect("temp directory");
        let binary_path = root.path().join("fake-firecracker");
        std::fs::write(&binary_path, "#!/bin/sh\nexec sleep 30\n").expect("write fake firecracker");
        set_executable(&binary_path);
        let vm_config = FirecrackerConfig::new(
            root.path().join("vmlinux"),
            root.path().join("rootfs.ext4"),
            root.path().join("guest-agent.sock"),
            1,
            128,
            3,
        );
        let process_config = FirecrackerProcessConfig::new(
            &binary_path,
            root.path().join("api.sock"),
            root.path().join("firecracker.json"),
        );
        let mut process =
            FirecrackerProcess::start(&process_config, &vm_config).expect("start fake firecracker");

        let error = process
            .wait_ready(Duration::from_millis(20))
            .await
            .expect_err("firecracker does not become ready");
        assert!(error.to_string().contains("did not become ready"));
        process.stop().await.expect("stop fake firecracker");
    }

    #[cfg(target_os = "linux")]
    fn set_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(path)
            .expect("fake firecracker metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("make fake firecracker executable");
    }

    #[cfg(target_os = "linux")]
    async fn wait_for_file(path: &Path) {
        for _ in 0..100 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("fake firecracker did not create {path:?}");
    }
}
