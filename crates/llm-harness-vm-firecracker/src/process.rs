use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

#[cfg(target_os = "linux")]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(target_os = "linux")]
use tokio::net::UnixStream;
#[cfg(target_os = "linux")]
use tokio::process::Child;

#[cfg(target_os = "linux")]
use crate::config::FirecrackerConfig;
use crate::config::FirecrackerConfigError;
#[cfg(target_os = "linux")]
use crate::image::open_directory;
#[cfg(target_os = "linux")]
use crate::jailer::FirecrackerJailerConfig;
#[cfg(target_os = "linux")]
use crate::jailer::FirecrackerJailerConfigError;

pub(crate) const PROCESS_PID_FILE_NAME: &str = "firecracker.pid";

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

    pub fn binary_path(&self) -> &Path {
        &self.binary_path
    }

    pub fn api_socket_path(&self) -> &Path {
        &self.api_socket_path
    }

    pub fn config_file_path(&self) -> &Path {
        &self.config_file_path
    }

    pub fn pid_file_path(&self) -> PathBuf {
        self.api_socket_path.with_file_name(PROCESS_PID_FILE_NAME)
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

#[cfg(target_os = "linux")]
impl From<FirecrackerJailerConfigError> for FirecrackerProcessError {
    fn from(error: FirecrackerJailerConfigError) -> Self {
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
    pid_file_path: PathBuf,
    vsock_socket_path: PathBuf,
    stopped: bool,
}

#[cfg(target_os = "linux")]
impl FirecrackerProcess {
    pub async fn stop_stale(
        config: &FirecrackerProcessConfig,
        rootfs_path: Option<&Path>,
        vsock_socket_path: &Path,
    ) -> Result<(), FirecrackerProcessError> {
        let pid_file_path = config.pid_file_path();
        let pid = read_pid_file(&pid_file_path)?;
        if process_exists(pid) {
            verify_process_identity(pid, config)?;
            const STOP_TIMEOUT: Duration = Duration::from_secs(2);
            const GRACEFUL_TIMEOUT: Duration = Duration::from_millis(500);
            let deadline = Instant::now() + STOP_TIMEOUT;
            if std::fs::symlink_metadata(&config.api_socket_path).is_ok() {
                let graceful = tokio::time::timeout(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(GRACEFUL_TIMEOUT),
                    send_ctrl_alt_del(&config.api_socket_path),
                )
                .await;
                if let Ok(Err(error)) = graceful {
                    tracing::warn!(pid, error = %error, "stale Firecracker graceful stop failed");
                }
                while process_exists(pid) && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            loop {
                match unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) } {
                    0 => {}
                    error => {
                        if error == libc::ESRCH {
                            break;
                        }
                        return Err(std::io::Error::last_os_error().into());
                    }
                }
                if Instant::now() >= deadline {
                    return Err(FirecrackerProcessError {
                        message: format!("stale firecracker process {pid} did not exit"),
                    });
                }
                if !process_exists(pid) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        remove_stale_artifacts(
            &config.api_socket_path,
            &config.config_file_path,
            rootfs_path,
            vsock_socket_path,
            &pid_file_path,
        )?;
        Ok(())
    }

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
        let pid_file_path = config.pid_file_path();
        match std::fs::symlink_metadata(&pid_file_path) {
            Ok(_) => {
                return Err(FirecrackerProcessError {
                    message: "PID file path already exists".to_owned(),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
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
        match std::fs::symlink_metadata(vm_config.vsock_uds_path()) {
            Ok(_) => {
                return Err(FirecrackerProcessError {
                    message: "vsock socket path already exists".to_owned(),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        write_config_file(&config.config_file_path, &config_json)?;

        let mut command = tokio::process::Command::new(&config.binary_path);
        command.args(config.command_args());
        command.kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = std::fs::remove_file(&config.config_file_path);
                return Err(error.into());
            }
        };
        let pid = child.id().ok_or_else(|| FirecrackerProcessError {
            message: "firecracker process has no PID".to_owned(),
        })?;
        if let Err(error) = write_pid_file(&pid_file_path, pid) {
            let _ = child.start_kill();
            let _ = remove_artifacts(
                &config.api_socket_path,
                &config.config_file_path,
                vm_config.vsock_uds_path(),
                &pid_file_path,
            );
            return Err(error.into());
        }
        Ok(Self {
            child,
            api_socket_path: config.api_socket_path.clone(),
            config_file_path: config.config_file_path.clone(),
            pid_file_path,
            vsock_socket_path: vm_config.vsock_uds_path().to_path_buf(),
            stopped: false,
        })
    }

    pub fn start_jailed(
        jailer_config: &FirecrackerJailerConfig,
        config: &FirecrackerProcessConfig,
        vm_config: &FirecrackerConfig,
    ) -> Result<Self, FirecrackerProcessError> {
        let jailer_args = jailer_config.command_args(config)?;
        let jail_root = jailer_config.chroot_dir()?;
        let config_json = serde_json::to_vec_pretty(&vm_config.to_jailed_json(&jail_root)?)?;

        let config_parent =
            config
                .config_file_path()
                .parent()
                .ok_or_else(|| FirecrackerProcessError {
                    message: "config file path must have a parent directory".to_owned(),
                })?;
        let api_socket_parent =
            config
                .api_socket_path()
                .parent()
                .ok_or_else(|| FirecrackerProcessError {
                    message: "API socket path must have a parent directory".to_owned(),
                })?;
        validate_jail_parent(config_parent, "config file")?;
        validate_jail_parent(api_socket_parent, "API socket")?;

        for (label, path) in [
            ("API socket", config.api_socket_path()),
            ("config file", config.config_file_path()),
            ("vsock socket", vm_config.vsock_uds_path()),
        ] {
            match std::fs::symlink_metadata(path) {
                Ok(_) => {
                    return Err(FirecrackerProcessError {
                        message: format!("{label} path already exists"),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let pid_file_path = config.pid_file_path();
        match std::fs::symlink_metadata(&pid_file_path) {
            Ok(_) => {
                return Err(FirecrackerProcessError {
                    message: "PID file path already exists".to_owned(),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        write_config_file(config.config_file_path(), &config_json)?;
        if let Err(error) = std::os::unix::fs::chown(
            config.config_file_path(),
            Some(jailer_config.uid()),
            Some(jailer_config.gid()),
        ) {
            let _ = std::fs::remove_file(config.config_file_path());
            return Err(error.into());
        }

        let mut command = tokio::process::Command::new(jailer_config.jailer_binary_path());
        command.args(jailer_args);
        command.kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = std::fs::remove_file(config.config_file_path());
                return Err(error.into());
            }
        };
        let pid = child.id().ok_or_else(|| FirecrackerProcessError {
            message: "jailer process has no PID".to_owned(),
        })?;
        if let Err(error) = write_pid_file(&pid_file_path, pid) {
            let _ = child.start_kill();
            let _ = remove_artifacts(
                config.api_socket_path(),
                config.config_file_path(),
                vm_config.vsock_uds_path(),
                &pid_file_path,
            );
            return Err(error.into());
        }
        Ok(Self {
            child,
            api_socket_path: config.api_socket_path().to_path_buf(),
            config_file_path: config.config_file_path().to_path_buf(),
            pid_file_path,
            vsock_socket_path: vm_config.vsock_uds_path().to_path_buf(),
            stopped: false,
        })
    }

    pub async fn stop(&mut self) -> Result<(), FirecrackerProcessError> {
        if self.stopped {
            return Ok(());
        }
        let _ = self.child.start_kill();
        let wait_result = self.child.wait().await;
        let cleanup_result = remove_artifacts(
            &self.api_socket_path,
            &self.config_file_path,
            &self.vsock_socket_path,
            &self.pid_file_path,
        );
        if wait_result.is_ok() && cleanup_result.is_ok() {
            self.stopped = true;
        }
        wait_result?;
        cleanup_result?;
        Ok(())
    }

    pub async fn graceful_stop(
        &mut self,
        timeout: Duration,
    ) -> Result<(), FirecrackerProcessError> {
        if self.stopped {
            return Ok(());
        }
        if self.child.try_wait()?.is_some() {
            let cleanup_result = remove_artifacts(
                &self.api_socket_path,
                &self.config_file_path,
                &self.vsock_socket_path,
                &self.pid_file_path,
            );
            match cleanup_result {
                Ok(()) => {
                    self.stopped = true;
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            }
        }

        let signal_result = tokio::time::timeout(timeout, send_ctrl_alt_del(&self.api_socket_path))
            .await
            .map_err(|_| FirecrackerProcessError {
                message: format!(
                    "firecracker graceful shutdown request timed out within {timeout:?}"
                ),
            })
            .and_then(|result| result);
        if let Err(error) = signal_result {
            if self.child.try_wait()?.is_some() {
                let cleanup_result = remove_artifacts(
                    &self.api_socket_path,
                    &self.config_file_path,
                    &self.vsock_socket_path,
                    &self.pid_file_path,
                );
                match cleanup_result {
                    Ok(()) => {
                        self.stopped = true;
                        return Ok(());
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            let stop_error = self.stop().await.err();
            return Err(graceful_stop_error(error, stop_error));
        }

        let wait_result = wait_for_exit(&mut self.child, timeout).await;
        if let Err(wait_error) = wait_result {
            let stop_error = self.stop().await.err();
            return Err(graceful_stop_error(wait_error, stop_error));
        }

        let cleanup_result = remove_artifacts(
            &self.api_socket_path,
            &self.config_file_path,
            &self.vsock_socket_path,
            &self.pid_file_path,
        );
        if cleanup_result.is_ok() {
            self.stopped = true;
            return Ok(());
        }
        Ok(cleanup_result?)
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
async fn send_ctrl_alt_del(api_socket_path: &Path) -> Result<(), FirecrackerProcessError> {
    let body = r#"{"action_type":"SendCtrlAltDel"}"#;
    let request = format!(
        "PUT /actions HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let mut stream = UnixStream::connect(api_socket_path).await?;
    stream.write_all(request.as_bytes()).await?;
    let mut response = Vec::new();
    let mut buffer = [0_u8; 256];
    loop {
        let bytes_read = stream.read(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }
        response.extend_from_slice(&buffer[..bytes_read]);
        if response.contains(&b'\n') || response.len() >= 1024 {
            break;
        }
    }
    let response = String::from_utf8_lossy(&response);
    let status_line = response
        .lines()
        .next()
        .ok_or_else(|| invalid_http_response("empty response"))?;
    if !status_line.starts_with("HTTP/1.1 2") {
        return Err(invalid_http_response(format!(
            "unexpected status line: {status_line}"
        )));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn wait_for_exit(
    child: &mut Child,
    timeout: Duration,
) -> Result<(), FirecrackerProcessError> {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(FirecrackerProcessError {
                message: format!("firecracker did not exit within {timeout:?}"),
            });
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(target_os = "linux")]
fn invalid_http_response(message: impl Into<String>) -> FirecrackerProcessError {
    FirecrackerProcessError {
        message: format!("invalid firecracker API response: {}", message.into()),
    }
}

#[cfg(target_os = "linux")]
fn graceful_stop_error(
    error: FirecrackerProcessError,
    stop_error: Option<FirecrackerProcessError>,
) -> FirecrackerProcessError {
    let mut message = format!("firecracker graceful stop failed: {error}");
    if let Some(stop_error) = stop_error {
        message.push_str(&format!("; force stop failed: {stop_error}"));
    }
    FirecrackerProcessError { message }
}

#[cfg(target_os = "linux")]
impl Drop for FirecrackerProcess {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        let _ = self.child.start_kill();
        let _ = remove_artifacts(
            &self.api_socket_path,
            &self.config_file_path,
            &self.vsock_socket_path,
            &self.pid_file_path,
        );
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
fn write_pid_file(path: &Path, pid: u32) -> std::io::Result<()> {
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(pid.to_string().as_bytes())?;
    file.sync_all()
}

#[cfg(target_os = "linux")]
fn read_pid_file(path: &Path) -> Result<u32, FirecrackerProcessError> {
    use std::io::Read;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    let mode = metadata.permissions().mode();
    if !metadata.is_file() || metadata.uid() != unsafe { libc::getuid() } || mode & 0o077 != 0 {
        return Err(FirecrackerProcessError {
            message: format!("firecracker PID file {path:?} is untrusted"),
        });
    }
    let mut contents = String::new();
    file.take(24).read_to_string(&mut contents)?;
    contents
        .trim()
        .parse()
        .map_err(|_| FirecrackerProcessError {
            message: format!("firecracker PID file {path:?} is invalid"),
        })
}

#[cfg(target_os = "linux")]
fn process_exists(pid: u32) -> bool {
    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
        return false;
    }
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return true;
    };
    status
        .split(") ")
        .nth(1)
        .and_then(|status_fields| status_fields.split_whitespace().next())
        != Some("Z")
}

#[cfg(target_os = "linux")]
fn verify_process_identity(
    pid: u32,
    config: &FirecrackerProcessConfig,
) -> Result<(), FirecrackerProcessError> {
    let expected_binary_metadata = std::fs::metadata(&config.binary_path)?;
    let actual_binary_metadata = std::fs::metadata(format!("/proc/{pid}/exe"))?;
    if expected_binary_metadata.dev() != actual_binary_metadata.dev()
        || expected_binary_metadata.ino() != actual_binary_metadata.ino()
    {
        return Err(FirecrackerProcessError {
            message: format!(
                "stale firecracker PID {pid} executable does not match {:?}",
                config.binary_path
            ),
        });
    }
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"))?;
    let arguments = cmdline
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .collect::<Vec<&[u8]>>();
    let executable = arguments
        .first()
        .and_then(|argument| {
            std::path::Path::new(std::ffi::OsStr::from_bytes(argument)).file_name()
        })
        .unwrap_or_default();
    let expected_executable = config
        .binary_path
        .file_name()
        .unwrap_or_default()
        .as_encoded_bytes();
    let api_flag = b"--api-sock".as_slice();
    let api_path = config.api_socket_path.as_os_str().as_encoded_bytes();
    let config_flag = b"--config-file".as_slice();
    let config_path = config.config_file_path.as_os_str().as_encoded_bytes();
    if executable.as_encoded_bytes() == expected_executable
        && arguments
            .windows(2)
            .any(|arguments| arguments[0] == api_flag && arguments[1] == api_path)
        && arguments
            .windows(2)
            .any(|arguments| arguments[0] == config_flag && arguments[1] == config_path)
    {
        return Ok(());
    }
    Err(FirecrackerProcessError {
        message: format!(
            "stale firecracker PID {pid} does not match {:?}",
            config.binary_path
        ),
    })
}

#[cfg(target_os = "linux")]
fn remove_artifacts(
    api_socket_path: &Path,
    config_file_path: &Path,
    vsock_socket_path: &Path,
    pid_file_path: &Path,
) -> std::io::Result<()> {
    let api_socket_error = remove_owned_file(api_socket_path).err();
    let config_file_error = remove_owned_file(config_file_path).err();
    let vsock_socket_error = remove_owned_file(vsock_socket_path).err();
    let pid_file_error = remove_owned_file(pid_file_path).err();
    if let Some(error) = api_socket_error
        .or(config_file_error)
        .or(vsock_socket_error)
        .or(pid_file_error)
    {
        return Err(error);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn remove_stale_artifacts(
    api_socket_path: &Path,
    config_file_path: &Path,
    rootfs_path: Option<&Path>,
    vsock_socket_path: &Path,
    pid_file_path: &Path,
) -> Result<(), FirecrackerProcessError> {
    if let Some(rootfs_path) = rootfs_path {
        remove_owned_regular_file(rootfs_path)?;
    }
    remove_artifacts(
        api_socket_path,
        config_file_path,
        vsock_socket_path,
        pid_file_path,
    )?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn remove_owned_regular_file(path: &Path) -> std::io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "stale rootfs is not a regular file",
        ));
    }
    std::fs::remove_file(path)
}

#[cfg(target_os = "linux")]
fn remove_owned_file(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn validate_jail_parent(path: &Path, label: &'static str) -> Result<(), FirecrackerProcessError> {
    open_directory(path)
        .map(|_| ())
        .map_err(|error| FirecrackerProcessError {
            message: format!("invalid {label} parent directory: {error}"),
        })
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
    #[cfg(target_os = "linux")]
    use crate::jailer::FirecrackerJailerConfig;

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
    #[test]
    fn start_rejects_an_existing_vsock_socket() {
        let root = tempfile::tempdir().expect("temp directory");
        let vsock_socket_path = root.path().join("guest-agent.sock");
        std::fs::write(&vsock_socket_path, b"unowned").expect("write existing vsock socket");
        let vm_config = FirecrackerConfig::new(
            root.path().join("vmlinux"),
            root.path().join("rootfs.ext4"),
            &vsock_socket_path,
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
            Ok(_) => panic!("existing vsock socket unexpectedly accepted"),
        };
        assert!(
            error
                .to_string()
                .contains("vsock socket path already exists")
        );
        assert_eq!(
            std::fs::read(&vsock_socket_path).expect("existing vsock socket remains"),
            b"unowned"
        );
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
    async fn graceful_stop_sends_ctrl_alt_del_and_cleans_artifacts() {
        let root = tempfile::tempdir().expect("temp directory");
        let binary_path = root.path().join("fake-firecracker");
        std::fs::write(
            &binary_path,
            b"#!/usr/bin/env python3\nimport socket, sys\nserver = socket.socket(socket.AF_UNIX)\nserver.bind(sys.argv[2])\nserver.listen(4)\nwhile True:\n    connection, _ = server.accept()\n    request = connection.recv(4096)\n    if request.startswith(b\"PUT /actions\"):\n        connection.sendall(b\"HTTP/1.1 204 No Content\\r\\nContent-Length: 0\\r\\nConnection: close\\r\\n\\r\\n\")\n        connection.close()\n        break\n    connection.close()\n",
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

        process
            .wait_ready(Duration::from_millis(500))
            .await
            .expect("fake firecracker becomes ready");
        process
            .graceful_stop(Duration::from_secs(2))
            .await
            .expect("graceful stop fake firecracker");
        assert!(!process_config.api_socket_path.exists());
        assert!(!process_config.config_file_path.exists());
        process
            .graceful_stop(Duration::from_secs(2))
            .await
            .expect("graceful stop is idempotent");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn graceful_stop_times_out_and_falls_back_to_force_stop() {
        let root = tempfile::tempdir().expect("temp directory");
        let binary_path = root.path().join("fake-firecracker");
        std::fs::write(
            &binary_path,
            b"#!/usr/bin/env python3\nimport socket, sys, time\nserver = socket.socket(socket.AF_UNIX)\nserver.bind(sys.argv[2])\nserver.listen(1)\ntime.sleep(30)\n",
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
        let error = process
            .graceful_stop(Duration::from_millis(50))
            .await
            .expect_err("fake firecracker ignores graceful shutdown");
        let message = error.to_string();
        assert!(
            message.contains("graceful shutdown request timed out"),
            "{message}"
        );
        assert!(!error.to_string().contains("force stop failed: "));
        assert!(!process_config.api_socket_path.exists());
        assert!(!process_config.config_file_path.exists());
        assert!(!process_config.pid_file_path().exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn stop_stale_kills_a_verified_process_and_removes_artifacts() {
        let root = tempfile::tempdir().expect("temp directory");
        let process_config = FirecrackerProcessConfig::new(
            "/usr/bin/python3",
            root.path().join("api.sock"),
            root.path().join("firecracker.json"),
        );
        let vsock_socket_path = root.path().join("guest-agent.sock");
        let rootfs_path = root.path().join("rootfs.ext4");
        let pid_file_path = process_config.pid_file_path();
        std::fs::write(&rootfs_path, b"stale rootfs").expect("write stale rootfs");

        let mut stale_process = std::process::Command::new("/usr/bin/python3")
            .arg("-c")
            .arg("import sys, time; time.sleep(30)")
            .arg("--api-sock")
            .arg(process_config.api_socket_path())
            .arg("--config-file")
            .arg(process_config.config_file_path())
            .spawn()
            .expect("start stale process");
        let pid = stale_process.id();
        std::fs::write(&pid_file_path, pid.to_string()).expect("write stale PID");
        set_private_file_permissions(&pid_file_path);

        let stop_result =
            FirecrackerProcess::stop_stale(&process_config, Some(&rootfs_path), &vsock_socket_path)
                .await;
        let _ = stale_process.wait();
        stop_result.expect("stop stale process");
        assert!(!process_exists(pid));
        assert!(!process_config.api_socket_path.exists());
        assert!(!process_config.config_file_path.exists());
        assert!(!pid_file_path.exists());
        assert!(!rootfs_path.exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn stop_stale_removes_artifacts_after_a_dead_pid() {
        let root = tempfile::tempdir().expect("temp directory");
        let process_config = FirecrackerProcessConfig::new(
            root.path().join("firecracker"),
            root.path().join("api.sock"),
            root.path().join("firecracker.json"),
        );
        let vsock_socket_path = root.path().join("guest-agent.sock");
        let rootfs_path = root.path().join("rootfs.ext4");
        let pid_file_path = process_config.pid_file_path();
        for path in [
            &rootfs_path,
            &process_config.api_socket_path,
            &process_config.config_file_path,
            &vsock_socket_path,
        ] {
            std::fs::write(path, b"stale").expect("write stale artifact");
        }
        std::fs::write(&pid_file_path, b"999999").expect("write dead PID");
        set_private_file_permissions(&pid_file_path);

        FirecrackerProcess::stop_stale(&process_config, Some(&rootfs_path), &vsock_socket_path)
            .await
            .expect("clean dead stale process");
        for path in [
            rootfs_path,
            process_config.api_socket_path,
            process_config.config_file_path,
            vsock_socket_path,
            pid_file_path,
        ] {
            assert!(!path.exists());
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn stop_stale_rejects_an_untrusted_pid_file() {
        let root = tempfile::tempdir().expect("temp directory");
        let process_config = FirecrackerProcessConfig::new(
            root.path().join("firecracker"),
            root.path().join("api.sock"),
            root.path().join("firecracker.json"),
        );
        let vsock_socket_path = root.path().join("guest-agent.sock");
        let pid_file_path = process_config.pid_file_path();
        std::fs::write(&pid_file_path, b"999999").expect("write PID file");
        std::fs::set_permissions(&pid_file_path, std::fs::Permissions::from_mode(0o644))
            .expect("set PID file permissions");

        let error = FirecrackerProcess::stop_stale(&process_config, None, &vsock_socket_path)
            .await
            .expect_err("reject an untrusted PID file");
        assert!(error.to_string().contains("is untrusted"), "{error}");
        assert!(pid_file_path.exists());
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
    #[tokio::test]
    async fn wait_ready_fails_after_stop() {
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
        process.stop().await.expect("stop fake firecracker");

        let error = process
            .wait_ready(Duration::from_millis(20))
            .await
            .expect_err("firecracker is stopped");
        assert!(error.to_string().contains("firecracker process is stopped"));
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
    fn jailed_process_environment() -> (
        tempfile::TempDir,
        FirecrackerJailerConfig,
        FirecrackerProcessConfig,
        FirecrackerConfig,
    ) {
        let root = tempfile::tempdir().expect("temp directory");
        let jailer_binary_path = root.path().join("fake-jailer");
        let firecracker_binary_path = root.path().join("firecracker");
        let chroot_base_dir = root.path().join("jail");
        let chroot_dir = chroot_base_dir
            .join("firecracker")
            .join("sandbox-1")
            .join("root");
        std::fs::create_dir_all(&chroot_dir).expect("create chroot directory");
        std::fs::write(&firecracker_binary_path, b"fake firecracker")
            .expect("write fake firecracker");
        std::fs::write(chroot_dir.join("vmlinux"), b"kernel").expect("stage kernel");
        std::fs::write(chroot_dir.join("rootfs.ext4"), b"rootfs").expect("stage rootfs");
        let (uid, gid) = non_root_ids();
        let jailer_script = format!(
            r#"#!/usr/bin/env python3
import socket, sys, time
args = sys.argv[1:]
api_path = args[args.index("--api-sock") + 1]
host_path = {:?} + api_path
server = socket.socket(socket.AF_UNIX)
server.bind(host_path)
server.listen(1)
time.sleep(30)
"#,
            chroot_dir
        );
        std::fs::write(&jailer_binary_path, jailer_script).expect("write fake jailer");
        set_executable(&jailer_binary_path);

        let jailer_config = FirecrackerJailerConfig::new(
            &jailer_binary_path,
            &firecracker_binary_path,
            "sandbox-1",
            uid,
            gid,
        )
        .with_chroot_base_dir(&chroot_base_dir);
        let process_config = FirecrackerProcessConfig::new(
            &firecracker_binary_path,
            chroot_dir.join("api.sock"),
            chroot_dir.join("firecracker.json"),
        );
        let vm_config = FirecrackerConfig::new(
            chroot_dir.join("vmlinux"),
            chroot_dir.join("rootfs.ext4"),
            chroot_dir.join("guest-agent.sock"),
            1,
            128,
            3,
        );
        (root, jailer_config, process_config, vm_config)
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn start_jailed_maps_paths_and_owns_the_config_file() {
        let (root, jailer_config, process_config, vm_config) = jailed_process_environment();
        let mut process =
            FirecrackerProcess::start_jailed(&jailer_config, &process_config, &vm_config)
                .expect("start fake jailer");

        let config_contents =
            std::fs::read(process_config.config_file_path()).expect("read jailed config file");
        let config_json: serde_json::Value =
            serde_json::from_slice(&config_contents).expect("parse jailed config file");
        assert_eq!(config_json["boot-source"]["kernel_image_path"], "/vmlinux");
        assert_eq!(config_json["drives"][0]["path_on_host"], "/rootfs.ext4");
        assert_eq!(config_json["vsock"]["uds_path"], "/guest-agent.sock");

        use std::os::unix::fs::MetadataExt;
        let metadata =
            std::fs::metadata(process_config.config_file_path()).expect("jailed config metadata");
        assert_eq!(metadata.uid(), jailer_config.uid());
        assert_eq!(metadata.gid(), jailer_config.gid());

        process
            .wait_ready(Duration::from_millis(500))
            .await
            .expect("fake jailed firecracker becomes ready");
        process.stop().await.expect("stop fake jailer");
        assert!(!process_config.api_socket_path().exists());
        assert!(!process_config.config_file_path().exists());
        assert!(
            root.path()
                .join("jail/firecracker/sandbox-1/root/vmlinux")
                .exists()
        );
        assert!(
            root.path()
                .join("jail/firecracker/sandbox-1/root/rootfs.ext4")
                .exists()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn start_jailed_rejects_an_existing_vsock_socket() {
        let (root, jailer_config, process_config, vm_config) = jailed_process_environment();
        let vsock_socket_path = vm_config.vsock_uds_path().to_path_buf();
        std::fs::write(&vsock_socket_path, b"unowned").expect("write existing vsock socket");

        let error =
            match FirecrackerProcess::start_jailed(&jailer_config, &process_config, &vm_config) {
                Err(error) => error,
                Ok(_) => panic!("existing jailed vsock socket unexpectedly accepted"),
            };
        assert!(
            error
                .to_string()
                .contains("vsock socket path already exists")
        );
        assert_eq!(
            std::fs::read(&vsock_socket_path).expect("existing vsock socket remains"),
            b"unowned"
        );
        assert!(!process_config.config_file_path.exists());
        drop(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn start_jailed_failure_removes_the_config_file() {
        use std::os::unix::fs::PermissionsExt;

        let (_root, jailer_config, process_config, vm_config) = jailed_process_environment();
        let mut permissions = std::fs::metadata(jailer_config.jailer_binary_path())
            .expect("fake jailer metadata")
            .permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(jailer_config.jailer_binary_path(), permissions)
            .expect("remove fake jailer execute permission");

        let error =
            match FirecrackerProcess::start_jailed(&jailer_config, &process_config, &vm_config) {
                Err(error) => error,
                Ok(_) => panic!("fake jailer unexpectedly started"),
            };
        assert!(error.to_string().contains("firecracker process io error"));
        assert!(!process_config.config_file_path().exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn start_jailed_rejects_vm_resources_outside_the_chroot() {
        let (root, jailer_config, process_config, _vm_config) = jailed_process_environment();
        let chroot_dir = root.path().join("jail/firecracker/sandbox-1/root");
        let vm_config = FirecrackerConfig::new(
            root.path().join("outside-vmlinux"),
            chroot_dir.join("rootfs.ext4"),
            chroot_dir.join("guest-agent.sock"),
            1,
            128,
            3,
        );

        let error =
            match FirecrackerProcess::start_jailed(&jailer_config, &process_config, &vm_config) {
                Err(error) => error,
                Ok(_) => panic!("jailed process unexpectedly started"),
            };
        assert!(
            error
                .to_string()
                .contains("kernel image host path must be inside the jail root"),
            "{error}"
        );
        assert!(!process_config.config_file_path().exists());
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
    fn set_private_file_permissions(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .expect("make PID file private");
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
