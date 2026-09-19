use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use url::{Host, Url};

pub const PANEL_DESCRIPTOR_LINE_PREFIX: &str = "RUNTIME_APP_PANEL_DESCRIPTOR=";
const PANEL_DESCRIPTOR_SCHEMA: &str = "llm-harness.studio.panel-descriptor.v1";
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TIMEOUT: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone)]
pub struct CoreProcessConfig {
    pub binary_path: PathBuf,
    pub data_root: Option<PathBuf>,
    pub startup_timeout: Duration,
    pub shutdown_timeout: Duration,
}

impl CoreProcessConfig {
    pub fn from_env() -> Result<Self, CoreProcessError> {
        let binary_path = match std::env::var("LLM_HARNESS_APP_BIN") {
            Ok(binary_path) => PathBuf::from(binary_path),
            Err(std::env::VarError::NotPresent) => default_binary_path()?,
            Err(error) => return Err(CoreProcessError::ReadEnvironment(error)),
        };
        let data_root = match std::env::var("RUNTIME_APP_DATA_ROOT") {
            Ok(data_root) => Some(PathBuf::from(data_root)),
            Err(std::env::VarError::NotPresent) => None,
            Err(error) => return Err(CoreProcessError::ReadEnvironment(error)),
        };

        Ok(Self {
            binary_path,
            data_root,
            startup_timeout: environment_duration(
                "RUNTIME_APP_SHELL_STARTUP_TIMEOUT_SECS",
                DEFAULT_STARTUP_TIMEOUT,
            )?,
            shutdown_timeout: environment_duration(
                "RUNTIME_APP_SHUTDOWN_TIMEOUT_SECS",
                DEFAULT_SHUTDOWN_TIMEOUT,
            )?,
        })
    }
}

fn environment_duration(name: &str, default: Duration) -> Result<Duration, CoreProcessError> {
    let raw_value = match std::env::var(name) {
        Ok(raw_value) => raw_value,
        Err(std::env::VarError::NotPresent) => return Ok(default),
        Err(error) => return Err(CoreProcessError::ReadEnvironment(error)),
    };
    let seconds = raw_value
        .parse::<u64>()
        .map_err(CoreProcessError::ParseEnvironmentDuration)?;
    let duration = Duration::from_secs(seconds);
    if duration.is_zero() || duration > MAX_TIMEOUT {
        return Err(CoreProcessError::InvalidEnvironmentDuration {
            name: name.to_owned(),
            value: seconds,
        });
    }
    Ok(duration)
}

pub fn default_binary_path() -> Result<PathBuf, CoreProcessError> {
    let binary_name = if cfg!(windows) {
        "llm-harness-app.exe"
    } else {
        "llm-harness-app"
    };
    let mut binary_path = std::env::current_exe().map_err(CoreProcessError::CurrentExecutable)?;
    binary_path.set_file_name(binary_name);
    Ok(binary_path)
}

pub fn parse_panel_descriptor_line(line: &str) -> Option<PathBuf> {
    line.strip_prefix(PANEL_DESCRIPTOR_LINE_PREFIX)
        .map(|path| path.trim_end_matches(['\n', '\r']).into())
}

#[derive(Debug)]
pub struct CoreProcess {
    child: Child,
    panel_url: Url,
    shutdown_timeout: Duration,
    shutdown_finished: bool,
}

impl CoreProcess {
    pub fn spawn(config: CoreProcessConfig) -> Result<Self, CoreProcessError> {
        let mut command = Command::new(&config.binary_path);
        command.env("RUNTIME_APP_PORT", "0").stdout(Stdio::piped());
        if let Some(data_root) = &config.data_root {
            command.env("RUNTIME_APP_DATA_ROOT", data_root);
        }
        #[cfg(windows)]
        {
            prepare_hidden_windows_console();
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            command.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }

        let mut child = command.spawn()?;
        let stdout = child.stdout.take().ok_or(CoreProcessError::CaptureStdout)?;
        let descriptor_path = match read_panel_descriptor_path(stdout, config.startup_timeout) {
            Ok(descriptor_path) => descriptor_path,
            Err(error) => {
                abandon_child(&mut child);
                return Err(error);
            }
        };
        if let Err(error) = validate_descriptor_path(&descriptor_path, config.data_root.as_deref())
        {
            abandon_child(&mut child);
            return Err(error);
        }
        let panel_url = match read_panel_descriptor(&descriptor_path) {
            Ok(panel_url) => panel_url,
            Err(error) => {
                abandon_child(&mut child);
                return Err(error);
            }
        };

        Ok(Self {
            child,
            panel_url,
            shutdown_timeout: config.shutdown_timeout,
            shutdown_finished: false,
        })
    }

    pub fn panel_url(&self) -> &Url {
        &self.panel_url
    }

    pub fn shutdown(mut self) -> ShutdownOutcome {
        self.shutdown_inner()
    }

    fn shutdown_inner(&mut self) -> ShutdownOutcome {
        self.shutdown_finished = true;
        if let Ok(Some(status)) = self.child.try_wait() {
            return ShutdownOutcome::AlreadyExited(status);
        }

        if send_shutdown_signal(&mut self.child) {
            let deadline = Instant::now() + self.shutdown_timeout;
            loop {
                match self.child.try_wait() {
                    Ok(Some(status)) => return ShutdownOutcome::Graceful(status),
                    Ok(None) if Instant::now() >= deadline => break,
                    Ok(None) => thread::sleep(Duration::from_millis(25)),
                    Err(_) => break,
                }
            }
        } else if let Ok(Some(status)) = self.child.try_wait() {
            return ShutdownOutcome::AlreadyExited(status);
        }

        let _ = self.child.kill();
        match self.child.wait() {
            Ok(status) => ShutdownOutcome::Forced(status),
            Err(_) => ShutdownOutcome::ForcedUnavailable,
        }
    }
}

impl Drop for CoreProcess {
    fn drop(&mut self) {
        if !self.shutdown_finished {
            let _ = self.shutdown_inner();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownOutcome {
    Graceful(ExitStatus),
    Forced(ExitStatus),
    AlreadyExited(ExitStatus),
    ForcedUnavailable,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn descriptor_path_must_match_data_root() {
        let root = TempDir::new().unwrap();
        let descriptor_path = root.path().join("panel.json");

        validate_descriptor_path(&descriptor_path, Some(root.path())).unwrap();
        validate_descriptor_path(&descriptor_path, None).unwrap();

        let unexpected_path = root.path().join("unexpected.json");
        assert!(matches!(
            validate_descriptor_path(&unexpected_path, Some(root.path())),
            Err(CoreProcessError::UnexpectedDescriptorPath { .. })
        ));
    }

    #[test]
    fn panel_url_must_be_http_loopback_ip() {
        assert!(panel_url_is_loopback(
            &Url::parse("http://127.0.0.1/app").unwrap()
        ));
        assert!(panel_url_is_loopback(
            &Url::parse("http://[::1]/app").unwrap()
        ));
        assert!(!panel_url_is_loopback(
            &Url::parse("https://127.0.0.1/app").unwrap()
        ));
        assert!(!panel_url_is_loopback(
            &Url::parse("http://localhost/app").unwrap()
        ));
        assert!(!panel_url_is_loopback(
            &Url::parse("http://example.com/app").unwrap()
        ));
    }
}

enum ReaderEvent {
    Descriptor(PathBuf),
    Eof,
    ReadError,
}

fn read_panel_descriptor_path(
    stdout: std::process::ChildStdout,
    startup_timeout: Duration,
) -> Result<PathBuf, CoreProcessError> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = sender.send(ReaderEvent::Eof);
                    break;
                }
                Ok(_) => {
                    if let Some(descriptor_path) = parse_panel_descriptor_line(&line) {
                        let _ = sender.send(ReaderEvent::Descriptor(descriptor_path));
                        break;
                    }
                }
                Err(_) => {
                    let _ = sender.send(ReaderEvent::ReadError);
                    break;
                }
            }
        }
    });

    match receiver.recv_timeout(startup_timeout) {
        Ok(ReaderEvent::Descriptor(descriptor_path)) => Ok(descriptor_path),
        Ok(ReaderEvent::Eof) => Err(CoreProcessError::MissingDescriptor),
        Ok(ReaderEvent::ReadError) => Err(CoreProcessError::ReadDescriptorLine),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            Err(CoreProcessError::StartupTimeout(startup_timeout))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(CoreProcessError::MissingDescriptor),
    }
}

#[derive(Debug, Deserialize)]
struct PanelDescriptorFile {
    schema: String,
    url: String,
}

pub fn read_panel_descriptor(path: &Path) -> Result<Url, CoreProcessError> {
    let file = std::fs::File::open(path)?;
    let descriptor: PanelDescriptorFile = serde_json::from_reader(file)?;
    if descriptor.schema != PANEL_DESCRIPTOR_SCHEMA {
        return Err(CoreProcessError::UnexpectedDescriptorSchema(
            descriptor.schema,
        ));
    }
    let panel_url = Url::parse(&descriptor.url)?;
    if !panel_url_is_loopback(&panel_url) {
        return Err(CoreProcessError::NonLoopbackPanelUrl(panel_url));
    }
    Ok(panel_url)
}

fn panel_url_is_loopback(panel_url: &Url) -> bool {
    if panel_url.scheme() != "http" {
        return false;
    }
    match panel_url.host() {
        Some(Host::Domain(_)) => false,
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        _ => false,
    }
}

fn validate_descriptor_path(
    descriptor_path: &Path,
    data_root: Option<&Path>,
) -> Result<(), CoreProcessError> {
    let Some(data_root) = data_root else {
        return Ok(());
    };
    let expected_descriptor_path = data_root.join("panel.json");
    if descriptor_path == expected_descriptor_path {
        Ok(())
    } else {
        Err(CoreProcessError::UnexpectedDescriptorPath {
            expected: expected_descriptor_path,
            actual: descriptor_path.to_path_buf(),
        })
    }
}

fn abandon_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
fn send_shutdown_signal(child: &mut Child) -> bool {
    let pid = child.id() as libc::pid_t;
    unsafe { libc::kill(pid, libc::SIGTERM) == 0 }
}

#[cfg(windows)]
fn send_shutdown_signal(child: &mut Child) -> bool {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};

    let process_id = child.id();
    unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, process_id) != 0 }
}

#[cfg(windows)]
fn prepare_hidden_windows_console() {
    use windows_sys::Win32::System::Console::{AllocConsole, GetConsoleWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};

    unsafe {
        if GetConsoleWindow().is_null() && AllocConsole() != 0 {
            let console_window = GetConsoleWindow();
            if !console_window.is_null() {
                ShowWindow(console_window, SW_HIDE);
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CoreProcessError {
    #[error("read environment variable failed: {0}")]
    ReadEnvironment(#[from] std::env::VarError),
    #[error("parse environment duration failed: {0}")]
    ParseEnvironmentDuration(#[from] std::num::ParseIntError),
    #[error("invalid environment duration {name}={value}")]
    InvalidEnvironmentDuration { name: String, value: u64 },
    #[error("resolve current executable failed: {0}")]
    CurrentExecutable(#[source] std::io::Error),
    #[error("spawn runtime app failed: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("capture runtime app stdout failed")]
    CaptureStdout,
    #[error("runtime app startup timed out after {0:?}")]
    StartupTimeout(Duration),
    #[error("runtime app exited without reporting panel descriptor")]
    MissingDescriptor,
    #[error("read runtime app stdout failed")]
    ReadDescriptorLine,
    #[error("unexpected panel descriptor schema: {0}")]
    UnexpectedDescriptorSchema(String),
    #[error("parse panel descriptor failed: {0}")]
    ParsePanelDescriptor(#[from] serde_json::Error),
    #[error("parse panel URL failed: {0}")]
    ParsePanelUrl(#[from] url::ParseError),
    #[error("panel URL is not local loopback: {0}")]
    NonLoopbackPanelUrl(Url),
    #[error("unexpected panel descriptor path: expected {expected}, got {actual}")]
    UnexpectedDescriptorPath { expected: PathBuf, actual: PathBuf },
}
