//! Linux VM guest agent binary.
//!
//! This binary binds an AF_VSOCK listener and serves the guest-agent protocol
//! for one workspace root at a time.

#[cfg(any(target_os = "linux", test))]
use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(target_os = "linux")]
use std::sync::Arc;

#[cfg(target_os = "linux")]
use llm_harness_vm_agent_protocol::ProtocolPeer;
#[cfg(target_os = "linux")]
use llm_harness_vm_agent_service::{GuestAgentService, WorkspaceAgentHandler};
#[cfg(target_os = "linux")]
use llm_harness_vm_agent_transport::FrameStream;
#[cfg(target_os = "linux")]
use llm_harness_vm_agent_transport::vsock::{VsockAddress, VsockListener};

#[cfg(any(target_os = "linux", test))]
const DEFAULT_ROOT: &str = "/workspace";
#[cfg(any(target_os = "linux", test))]
const DEFAULT_CID: u32 = u32::MAX;
#[cfg(any(target_os = "linux", test))]
const DEFAULT_PORT: u32 = 6000;

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct Config {
    root: PathBuf,
    cid: u32,
    port: u32,
}

#[cfg(any(target_os = "linux", test))]
impl Default for Config {
    fn default() -> Self {
        Self {
            root: PathBuf::from(DEFAULT_ROOT),
            cid: DEFAULT_CID,
            port: DEFAULT_PORT,
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Config, String> {
    let mut config = Config::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!(
                    "Usage: llm-harness-vm-agent [--root <path>] [--cid <cid>] [--port <port>]"
                );
                std::process::exit(0);
            }
            "--root" => {
                let value = args
                    .next()
                    .ok_or_else(|| "missing value for --root".to_owned())?;
                config.root = PathBuf::from(value);
            }
            "--cid" => {
                let value = args
                    .next()
                    .ok_or_else(|| "missing value for --cid".to_owned())?;
                config.cid = value.parse().map_err(|_| "invalid CID".to_owned())?;
            }
            "--port" => {
                let value = args
                    .next()
                    .ok_or_else(|| "missing value for --port".to_owned())?;
                config.port = value.parse().map_err(|_| "invalid port".to_owned())?;
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }
    if config.port == 0 {
        return Err("port must be nonzero".to_owned());
    }
    Ok(config)
}

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let config = match parse_args(std::env::args().skip(1)) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("llm-harness-vm-agent: {error}");
            return ExitCode::from(2);
        }
    };

    let handler = match WorkspaceAgentHandler::new(&config.root) {
        Ok(handler) => Arc::new(handler),
        Err(error) => {
            eprintln!(
                "llm-harness-vm-agent: open workspace root: {}",
                error.message()
            );
            return ExitCode::from(1);
        }
    };
    let address = match VsockAddress::new(config.cid, config.port) {
        Ok(address) => address,
        Err(error) => {
            eprintln!("llm-harness-vm-agent: invalid vsock address: {error}");
            return ExitCode::from(2);
        }
    };
    let listener = match VsockListener::bind(address) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("llm-harness-vm-agent: bind vsock listener: {error}");
            return ExitCode::from(1);
        }
    };

    eprintln!(
        "llm-harness-vm-agent: listening on vsock cid={} port={}",
        config.cid, config.port
    );

    loop {
        let stream = match listener.accept().await {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("llm-harness-vm-agent: accept vsock connection: {error}");
                continue;
            }
        };
        let frame_stream = match FrameStream::with_default_limit(stream, ProtocolPeer::Guest) {
            Ok(frame_stream) => frame_stream,
            Err(error) => {
                eprintln!("llm-harness-vm-agent: create guest stream: {error}");
                continue;
            }
        };
        let service = GuestAgentService::shared(Arc::clone(&handler));
        if let Err(error) = service.serve(frame_stream).await {
            eprintln!("llm-harness-vm-agent: connection error: {error}");
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("llm-harness-vm-agent: this binary requires Linux AF_VSOCK support");
    ExitCode::from(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_to_workspace_and_any_cid() {
        let config = Config::default();
        assert_eq!(config.root, PathBuf::from("/workspace"));
        assert_eq!(config.cid, u32::MAX);
        assert_eq!(config.port, 6000);
    }

    #[test]
    fn parses_custom_configuration() {
        let args = vec![
            "--root".to_owned(),
            "/tmp/root".to_owned(),
            "--cid".to_owned(),
            "42".to_owned(),
            "--port".to_owned(),
            "7000".to_owned(),
        ];
        let config = parse_args(args).expect("valid configuration");
        assert_eq!(config.root, PathBuf::from("/tmp/root"));
        assert_eq!(config.cid, 42);
        assert_eq!(config.port, 7000);
    }

    #[test]
    fn rejects_missing_and_invalid_values() {
        assert!(parse_args(vec!["--root".to_owned()]).is_err());
        assert!(parse_args(vec!["--cid".to_owned(), "bad".to_owned()]).is_err());
        assert!(parse_args(vec!["--port".to_owned(), "0".to_owned()]).is_err());
        assert!(parse_args(vec!["unknown".to_owned()]).is_err());
    }
}
