#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

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
        Ok(())
    }

    pub fn command_args(&self) -> Vec<String> {
        vec![
            "--api-sock".to_owned(),
            self.api_socket_path.to_string_lossy().into_owned(),
            "--config-file".to_owned(),
            self.config_file_path.to_string_lossy().into_owned(),
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
        #[cfg(target_os = "linux")]
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&config.config_file_path)?;
            file.write_all(&config_json)?;
        }

        let mut command = tokio::process::Command::new(&config.binary_path);
        command.args(config.command_args());
        command.kill_on_drop(true);
        let child = command.spawn()?;
        Ok(Self {
            child,
            api_socket_path: config.api_socket_path.clone(),
            config_file_path: config.config_file_path.clone(),
        })
    }

    pub async fn stop(&mut self) -> Result<(), FirecrackerProcessError> {
        self.child.start_kill()?;
        self.child.wait().await?;
        Ok(())
    }

    pub fn api_socket_path(&self) -> &Path {
        &self.api_socket_path
    }

    pub fn config_file_path(&self) -> &Path {
        &self.config_file_path
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
    Ok(())
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
                "--api-sock".to_owned(),
                std::env::temp_dir()
                    .join("llm-harness-api.sock")
                    .to_string_lossy()
                    .to_string(),
                "--config-file".to_owned(),
                std::env::temp_dir()
                    .join("llm-harness-firecracker.json")
                    .to_string_lossy()
                    .to_string(),
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
}
