//! Unix process supervision for a Studio backend.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::{Instant, timeout};

const DESCRIPTOR_PREFIX: &str = "STUDIO_PANEL_DESCRIPTOR=";
const DESCRIPTOR_SCHEMA: &str = "llm-harness.studio.panel-descriptor.v1";
const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("backend exited before readiness: {0:#}")]
    BackendExit(ExitStatus),
    #[error("backend process id is invalid: {0}")]
    Pid(#[from] std::num::TryFromIntError),
    #[error("backend descriptor is invalid: {0}")]
    Descriptor(#[from] serde_json::Error),
    #[error("backend did not become ready within {timeout_secs}s")]
    ReadinessTimeout { timeout_secs: u64 },
    #[error("backend descriptor path mismatch: expected {expected:?}, got {actual:?}")]
    DescriptorPath { expected: PathBuf, actual: PathBuf },
    #[error("backend descriptor has unexpected schema: {0}")]
    DescriptorSchema(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug)]
pub struct StudioHostConfig {
    program: PathBuf,
    data_root: PathBuf,
    port: u16,
    ready_timeout: Duration,
    shutdown_grace: Duration,
}

impl StudioHostConfig {
    pub fn new(program: impl Into<PathBuf>, data_root: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            data_root: data_root.into(),
            port: 0,
            ready_timeout: DEFAULT_READY_TIMEOUT,
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
        }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn with_ready_timeout(mut self, ready_timeout: Duration) -> Self {
        self.ready_timeout = ready_timeout;
        self
    }

    pub fn with_shutdown_grace(mut self, shutdown_grace: Duration) -> Self {
        self.shutdown_grace = shutdown_grace;
        self
    }
}

#[derive(Debug)]
pub struct StudioBackendHost {
    child: Option<Child>,
    data_root: PathBuf,
    descriptor_path: PathBuf,
    ready_timeout: Duration,
    shutdown_grace: Duration,
}

impl StudioBackendHost {
    pub fn spawn(config: StudioHostConfig) -> Result<Self, HostError> {
        let StudioHostConfig {
            program,
            data_root,
            port,
            ready_timeout,
            shutdown_grace,
        } = config;
        let mut command = Command::new(program);
        command.as_std_mut().process_group(0);
        command
            .arg_default_environment()
            .env("STUDIO_DATA_ROOT", &data_root)
            .env("STUDIO_PORT", port.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);
        let child = command.spawn()?;

        Ok(Self {
            descriptor_path: data_root.join("panel.json"),
            child: Some(child),
            data_root,
            ready_timeout,
            shutdown_grace,
        })
    }

    pub fn backend_pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    pub fn descriptor_path(&self) -> &Path {
        &self.descriptor_path
    }

    pub async fn wait_ready(mut self) -> Result<ReadyStudioBackend, HostError> {
        let timeout_secs = self.ready_timeout.as_secs();
        let deadline = Instant::now() + self.ready_timeout;
        let Some(child) = self.child.as_mut() else {
            return Err(HostError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "backend is not running",
            )));
        };
        let Some(stdout) = child.stdout.take() else {
            return Err(HostError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "backend stdout is unavailable",
            )));
        };
        let mut stdout = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(HostError::ReadinessTimeout { timeout_secs });
            }
            match timeout(remaining, stdout.read_line(&mut line)).await {
                Err(_) => return Err(HostError::ReadinessTimeout { timeout_secs }),
                Ok(Err(error)) => return Err(HostError::Io(error)),
                Ok(Ok(0)) => {
                    let descriptor_path = self.descriptor_path.clone();
                    let Some(mut child) = self.child.take() else {
                        return Err(HostError::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "backend is not running",
                        )));
                    };
                    let status = child.wait().await?;
                    clear_panel_descriptor(&descriptor_path);
                    return Err(HostError::BackendExit(status));
                }
                Ok(Ok(_)) => {
                    if line.starts_with(DESCRIPTOR_PREFIX) {
                        break;
                    }
                    line.clear();
                }
            }
        }

        let reported_path = PathBuf::from(
            line.strip_prefix(DESCRIPTOR_PREFIX)
                .expect("descriptor prefix was checked")
                .trim_end_matches(['\n', '\r']),
        );
        if reported_path != self.descriptor_path {
            self.shutdown().await?;
            clear_panel_descriptor(&self.data_root);
            return Err(HostError::DescriptorPath {
                expected: self.descriptor_path.clone(),
                actual: reported_path,
            });
        }
        if let Err(error) = self.validate_descriptor() {
            self.shutdown().await?;
            clear_panel_descriptor(&self.data_root);
            return Err(error);
        }

        Ok(ReadyStudioBackend {
            child: self.child.take(),
            data_root: self.data_root.clone(),
            descriptor_path: self.descriptor_path.clone(),
            shutdown_grace: self.shutdown_grace,
        })
    }

    fn validate_descriptor(&self) -> Result<(), HostError> {
        let contents = std::fs::read_to_string(&self.descriptor_path)?;
        let descriptor: serde_json::Value = serde_json::from_str(&contents)?;
        let schema = descriptor["schema"].as_str().unwrap_or_default();
        if schema != DESCRIPTOR_SCHEMA {
            return Err(HostError::DescriptorSchema(schema.to_owned()));
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<ExitStatus, HostError> {
        let Some(mut child) = self.child.take() else {
            return Err(HostError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "backend is not running",
            )));
        };
        if child.try_wait()?.is_some() {
            return Ok(child.wait().await?);
        }
        let Some(pid) = child.id() else {
            return Ok(child.wait().await?);
        };
        let result = unsafe { libc::killpg(pid.try_into()?, libc::SIGTERM) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(HostError::Io(error));
            }
        }
        match timeout(self.shutdown_grace, child.wait()).await {
            Ok(status) => Ok(status?),
            Err(_) => {
                child.start_kill()?;
                Ok(child.wait().await?)
            }
        }
    }
}

impl Drop for StudioBackendHost {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

#[derive(Debug)]
pub struct ReadyStudioBackend {
    child: Option<Child>,
    data_root: PathBuf,
    descriptor_path: PathBuf,
    shutdown_grace: Duration,
}

impl ReadyStudioBackend {
    pub fn backend_pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    pub fn descriptor_path(&self) -> &Path {
        &self.descriptor_path
    }

    pub async fn wait(mut self) -> Result<ExitStatus, HostError> {
        let Some(mut child) = self.child.take() else {
            return Err(HostError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "backend is not running",
            )));
        };
        let status = child.wait().await?;
        clear_panel_descriptor(&self.data_root);
        Ok(status)
    }

    pub async fn shutdown(mut self) -> Result<BackendShutdown, HostError> {
        let Some(mut child) = self.child.take() else {
            return Err(HostError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "backend is not running",
            )));
        };
        if child.try_wait()?.is_some() {
            let status = child.wait().await?;
            clear_panel_descriptor(&self.data_root);
            return Ok(BackendShutdown {
                status,
                forced: false,
            });
        }
        let Some(pid) = child.id() else {
            let status = child.wait().await?;
            clear_panel_descriptor(&self.data_root);
            return Ok(BackendShutdown {
                status,
                forced: false,
            });
        };
        let result = unsafe { libc::killpg(pid.try_into()?, libc::SIGTERM) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(HostError::Io(error));
            }
        }
        match timeout(self.shutdown_grace, child.wait()).await {
            Ok(status) => {
                let status = status?;
                clear_panel_descriptor(&self.data_root);
                Ok(BackendShutdown {
                    status,
                    forced: false,
                })
            }
            Err(_) => {
                let pid = child.id();
                child.start_kill()?;
                if let Some(pid) = pid
                    && let Ok(process_group) = pid.try_into()
                {
                    let _ = unsafe { libc::killpg(process_group, libc::SIGKILL) };
                }
                let status = child.wait().await?;
                clear_panel_descriptor(&self.data_root);
                Ok(BackendShutdown {
                    status,
                    forced: true,
                })
            }
        }
    }
}

fn clear_panel_descriptor(data_root: &Path) {
    let _ = std::fs::remove_file(data_root.join("panel.json"));
    let _ = std::fs::remove_file(data_root.join("panel.json.tmp"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn graceful_shutdown_escalates_to_force_kill() {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "agent-studio-host-force-kill-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let descriptor = root.join("panel.json");
        std::fs::write(
            &descriptor,
            serde_json::json!({"schema": DESCRIPTOR_SCHEMA}).to_string(),
        )
        .unwrap();
        let script = root.join("backend.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ntrap '' TERM INT\nprintf '{DESCRIPTOR_PREFIX}%s\\n' '{}'\nwhile :; do sleep 0.1; done\n",
                descriptor.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = StudioBackendHost::spawn(
            StudioHostConfig::new(&script, &root).with_shutdown_grace(Duration::from_millis(50)),
        )
        .unwrap()
        .wait_ready()
        .await
        .unwrap();
        let shutdown = backend.shutdown().await.unwrap();
        assert!(shutdown.forced);
        assert!(!shutdown.status.success());
        assert!(!descriptor.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendShutdown {
    pub status: ExitStatus,
    pub forced: bool,
}

trait CommandEnvironment {
    fn arg_default_environment(&mut self) -> &mut Self;
}

impl CommandEnvironment for Command {
    fn arg_default_environment(&mut self) -> &mut Self {
        for key in [
            "OPENAI_API_KEY",
            "OPENAI_API_BASE",
            "STUDIO_API_KEY",
            "STUDIO_DATA_ROOT",
            "STUDIO_PORT",
        ] {
            self.env_remove(key);
        }
        self
    }
}
