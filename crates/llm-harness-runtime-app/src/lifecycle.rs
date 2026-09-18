use std::fmt;
use std::future::Future;
use std::time::Duration;

const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3600);
#[cfg(windows)]
const WINDOWS_CONSOLE_CLOSE_SHUTDOWN_BUDGET: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownSource {
    CtrlC,
    #[cfg(unix)]
    Sigterm,
    #[cfg(windows)]
    CtrlBreak,
    #[cfg(windows)]
    CtrlClose,
}

impl ShutdownSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CtrlC => "ctrl_c",
            #[cfg(unix)]
            Self::Sigterm => "sigterm",
            #[cfg(windows)]
            Self::CtrlBreak => "ctrl_break",
            #[cfg(windows)]
            Self::CtrlClose => "ctrl_close",
        }
    }

    pub fn effective_shutdown_timeout(self, configured_timeout: Duration) -> Duration {
        #[cfg(windows)]
        if matches!(self, Self::CtrlClose) {
            return configured_timeout.min(WINDOWS_CONSOLE_CLOSE_SHUTDOWN_BUDGET);
        }
        configured_timeout
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownOutcome {
    Completed,
    TimedOut,
}

#[derive(Debug, Clone, Copy)]
pub struct LifecycleConfig {
    shutdown_timeout: Duration,
}

pub struct ShutdownSignalHandler {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(windows)]
    ctrl_c: tokio::signal::windows::CtrlC,
    #[cfg(windows)]
    ctrl_break: tokio::signal::windows::CtrlBreak,
    #[cfg(windows)]
    ctrl_close: tokio::signal::windows::CtrlClose,
}

impl ShutdownSignalHandler {
    pub fn install() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};

            Ok(Self {
                interrupt: signal(SignalKind::interrupt())?,
                terminate: signal(SignalKind::terminate())?,
            })
        }
        #[cfg(windows)]
        {
            use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close};

            Ok(Self {
                ctrl_c: ctrl_c()?,
                ctrl_break: ctrl_break()?,
                ctrl_close: ctrl_close()?,
            })
        }
    }

    pub async fn wait(&mut self) -> ShutdownSource {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.interrupt.recv() => ShutdownSource::CtrlC,
                _ = self.terminate.recv() => ShutdownSource::Sigterm,
            }
        }
        #[cfg(windows)]
        {
            tokio::select! {
                _ = self.ctrl_c.recv() => ShutdownSource::CtrlC,
                _ = self.ctrl_break.recv() => ShutdownSource::CtrlBreak,
                _ = self.ctrl_close.recv() => ShutdownSource::CtrlClose,
            }
        }
    }
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
        }
    }
}

impl LifecycleConfig {
    pub fn from_shutdown_timeout(shutdown_timeout: Duration) -> Result<Self, LifecycleConfigError> {
        if shutdown_timeout < Duration::from_secs(1) || shutdown_timeout > MAX_SHUTDOWN_TIMEOUT {
            return Err(LifecycleConfigError::InvalidShutdownTimeout(
                shutdown_timeout,
            ));
        }
        Ok(Self { shutdown_timeout })
    }

    pub fn from_env() -> Result<Self, LifecycleConfigError> {
        let raw_timeout = match std::env::var("RUNTIME_APP_SHUTDOWN_TIMEOUT_SECS") {
            Ok(raw_timeout) => raw_timeout,
            Err(std::env::VarError::NotPresent) => return Ok(Self::default()),
            Err(error) => return Err(LifecycleConfigError::ReadEnvironment(error)),
        };
        let seconds = raw_timeout
            .parse::<u64>()
            .map_err(LifecycleConfigError::ParseShutdownTimeout)?;
        Self::from_shutdown_timeout(Duration::from_secs(seconds))
    }

    pub const fn shutdown_timeout(self) -> Duration {
        self.shutdown_timeout
    }
}

#[derive(Debug)]
pub enum LifecycleConfigError {
    ReadEnvironment(std::env::VarError),
    ParseShutdownTimeout(std::num::ParseIntError),
    InvalidShutdownTimeout(Duration),
}

impl fmt::Display for LifecycleConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadEnvironment(error) => {
                write!(
                    formatter,
                    "read shutdown timeout environment failed: {error}"
                )
            }
            Self::ParseShutdownTimeout(error) => {
                write!(formatter, "parse shutdown timeout failed: {error}")
            }
            Self::InvalidShutdownTimeout(timeout) => write!(
                formatter,
                "invalid shutdown timeout {:?}; expected between 1s and 3600s",
                timeout
            ),
        }
    }
}

impl std::error::Error for LifecycleConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ReadEnvironment(error) => Some(error),
            Self::ParseShutdownTimeout(error) => Some(error),
            Self::InvalidShutdownTimeout(_) => None,
        }
    }
}

pub async fn wait_for_shutdown_signal() -> std::io::Result<ShutdownSource> {
    let mut handler = ShutdownSignalHandler::install()?;
    Ok(handler.wait().await)
}

pub async fn shutdown_with_timeout<F>(shutdown: F, timeout: Duration) -> ShutdownOutcome
where
    F: Future<Output = ()>,
{
    match tokio::time::timeout(timeout, shutdown).await {
        Ok(()) => ShutdownOutcome::Completed,
        Err(_) => ShutdownOutcome::TimedOut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::pending;

    #[test]
    fn lifecycle_config_accepts_the_supported_timeout_range() {
        assert_eq!(
            LifecycleConfig::from_shutdown_timeout(Duration::from_secs(1))
                .unwrap()
                .shutdown_timeout(),
            Duration::from_secs(1)
        );
        assert_eq!(
            LifecycleConfig::from_shutdown_timeout(MAX_SHUTDOWN_TIMEOUT)
                .unwrap()
                .shutdown_timeout(),
            MAX_SHUTDOWN_TIMEOUT
        );
        assert!(LifecycleConfig::from_shutdown_timeout(Duration::ZERO).is_err());
        assert!(LifecycleConfig::from_shutdown_timeout(Duration::from_millis(500)).is_err());
        assert!(
            LifecycleConfig::from_shutdown_timeout(MAX_SHUTDOWN_TIMEOUT + Duration::from_secs(1))
                .is_err()
        );
    }

    #[tokio::test]
    async fn shutdown_reports_completion_and_timeout() {
        assert_eq!(
            shutdown_with_timeout(std::future::ready(()), Duration::from_secs(1)).await,
            ShutdownOutcome::Completed
        );
        assert_eq!(
            shutdown_with_timeout(pending(), Duration::from_millis(10)).await,
            ShutdownOutcome::TimedOut
        );
    }

    #[test]
    fn shutdown_sources_have_stable_names() {
        assert_eq!(ShutdownSource::CtrlC.as_str(), "ctrl_c");
        #[cfg(unix)]
        assert_eq!(ShutdownSource::Sigterm.as_str(), "sigterm");
        #[cfg(windows)]
        {
            assert_eq!(ShutdownSource::CtrlBreak.as_str(), "ctrl_break");
            assert_eq!(ShutdownSource::CtrlClose.as_str(), "ctrl_close");
            assert_eq!(
                ShutdownSource::CtrlClose.effective_shutdown_timeout(Duration::from_secs(30)),
                WINDOWS_CONSOLE_CLOSE_SHUTDOWN_BUDGET
            );
        }
    }
}
