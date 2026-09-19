//! Fail-closed wire protocol for the VM sandbox guest agent.
//!
//! The protocol is intentionally independent of HTTP, vsock transport
//! implementations, and the runtime's execution environment. Both the gateway
//! and the guest agent can therefore depend on this crate without pulling the
//! other side's transport dependencies into the VM image.

use std::collections::BTreeMap;

use base64::Engine;
use serde::{Deserialize, Serialize};

/// Maximum payload accepted by a newly constructed decoder.
pub const DEFAULT_MAX_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
/// Maximum guest-enforced output cap accepted by protocol validation.
pub const MAX_PROCESS_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
/// Number of bytes in a protocol frame header.
pub const FRAME_HEADER_LEN: usize = 20;

const MAGIC: [u8; 4] = *b"LHSV";
const PROTOCOL_VERSION: u8 = 1;

/// The protocol peer that originated a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolPeer {
    /// The trusted gateway control process.
    Host,
    /// The guest agent, which is untrusted after workload execution.
    Guest,
}

/// A protocol encoding or decoding failure.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    /// A message violated a protocol invariant before encoding.
    #[error("invalid guest agent message: {0}")]
    InvalidMessage(&'static str),
    /// A frame payload exceeded the configured limit.
    #[error("guest agent frame payload {actual} exceeds limit {limit}")]
    PayloadTooLarge {
        /// Payload length reported by the frame header.
        actual: usize,
        /// Configured maximum payload length.
        limit: usize,
    },
    /// The frame header was malformed.
    #[error("invalid guest agent frame header: {0}")]
    InvalidHeader(&'static str),
    /// A frame body could not be decoded as a valid message.
    #[error("invalid guest agent frame body: {0}")]
    InvalidBody(#[from] serde_json::Error),
}

/// File content exchanged with a guest agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FileContent {
    /// UTF-8 text content.
    Text {
        /// Decoded text value.
        value: String,
    },
    /// Standard-base64-encoded binary content.
    Binary {
        /// Standard-base64-encoded bytes.
        base64: String,
    },
}

/// Metadata for a file or directory entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    /// Guest-relative workspace path.
    pub path: String,
    /// Whether the entry is a directory.
    pub is_dir: bool,
    /// Entry size in bytes.
    pub size: u64,
    /// Last modification time in Unix milliseconds.
    pub modified_ms: u64,
}

/// Final process result reported by the guest agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessResult {
    /// Process exit code.
    pub exit_code: i32,
    /// Number of stdout bytes discarded by the guest limit.
    pub stdout_truncated_bytes: u64,
    /// Number of stderr bytes discarded by the guest limit.
    pub stderr_truncated_bytes: u64,
}

/// A host request, guest response, or process event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AgentMessage {
    /// Check guest-agent liveness.
    Ping,
    /// Successful response to [`AgentMessage::Ping`].
    Pong,
    /// Request guest-agent status.
    Status,
    /// Response to [`AgentMessage::Status`].
    StatusResult {
        /// Guest-agent semantic version.
        agent_version: String,
        /// Number of processes currently tracked by the agent.
        active_processes: u32,
    },
    /// Read a file below `/workspace`.
    ReadFile {
        /// Guest-relative workspace path.
        path: String,
        /// Whether binary content is requested.
        binary: bool,
    },
    /// Response to [`AgentMessage::ReadFile`].
    ReadFileResult {
        /// File content.
        content: FileContent,
    },
    /// Write or append to a file below `/workspace`.
    WriteFile {
        /// Guest-relative workspace path.
        path: String,
        /// Content to write.
        content: FileContent,
        /// Append instead of truncating.
        append: bool,
    },
    /// Successful response to [`AgentMessage::WriteFile`].
    WriteFileResult,
    /// Request file or directory metadata.
    FileInfo {
        /// Guest-relative workspace path.
        path: String,
    },
    /// Response to [`AgentMessage::FileInfo`].
    FileInfoResult {
        /// File or directory metadata.
        entry: FileEntry,
    },
    /// List a directory below `/workspace`.
    ListDir {
        /// Guest-relative workspace path.
        path: String,
    },
    /// Response to [`AgentMessage::ListDir`].
    ListDirResult {
        /// Directory entries.
        entries: Vec<FileEntry>,
    },
    /// Check whether a path exists.
    Exists {
        /// Guest-relative workspace path.
        path: String,
    },
    /// Response to [`AgentMessage::Exists`].
    ExistsResult {
        /// Whether the requested path exists.
        exists: bool,
    },
    /// Create a directory below `/workspace`.
    CreateDir {
        /// Guest-relative workspace path.
        path: String,
        /// Create missing parent directories.
        recursive: bool,
    },
    /// Successful response to [`AgentMessage::CreateDir`].
    CreateDirResult,
    /// Remove a path below `/workspace`.
    Remove {
        /// Guest-relative workspace path.
        path: String,
        /// Remove directories recursively.
        recursive: bool,
        /// Ignore a missing path.
        force: bool,
    },
    /// Successful response to [`AgentMessage::Remove`].
    RemoveResult,
    /// Create a temporary directory below `/workspace`.
    CreateTempDir {
        /// Directory-name prefix.
        prefix: String,
    },
    /// Response to [`AgentMessage::CreateTempDir`].
    CreateTempDirResult {
        /// Created guest-relative workspace path.
        path: String,
    },
    /// Clean guest-owned process and temporary state.
    Cleanup,
    /// Successful response to [`AgentMessage::Cleanup`].
    CleanupResult,
    /// Start a process in the guest.
    StartProcess {
        /// Shell command to execute.
        command: String,
        /// Guest-relative working directory.
        cwd: Option<String>,
        /// Environment variables for the process.
        env: BTreeMap<String, String>,
        /// Guest-enforced process timeout.
        timeout_ms: Option<u64>,
        /// Guest-enforced output cap in bytes.
        max_output_bytes: u64,
    },
    /// Response to [`AgentMessage::StartProcess`].
    ProcessStarted {
        /// Gateway-assigned execution identifier.
        execution_id: String,
        /// Guest process identifier.
        pid: u32,
    },
    /// Cancel a tracked process.
    CancelProcess {
        /// Gateway-assigned execution identifier.
        execution_id: String,
        /// Whether ungraceful termination is allowed.
        force: bool,
    },
    /// Response to [`AgentMessage::CancelProcess`].
    CancelProcessResult {
        /// Whether a live process was cancelled.
        cancelled: bool,
    },
    /// Process stdout event.
    ProcessStdout {
        /// Gateway-assigned execution identifier.
        execution_id: String,
        /// UTF-8 output chunk.
        chunk: String,
    },
    /// Process stderr event.
    ProcessStderr {
        /// Gateway-assigned execution identifier.
        execution_id: String,
        /// UTF-8 output chunk.
        chunk: String,
    },
    /// Process exit event.
    ProcessExit {
        /// Gateway-assigned execution identifier.
        execution_id: String,
        /// Final exit code and output-truncation counts.
        result: ProcessResult,
    },
    /// Guest-enforced timeout event.
    ProcessTimeout {
        /// Gateway-assigned execution identifier.
        execution_id: String,
        /// Timeout exit code and output-truncation counts.
        result: ProcessResult,
    },
    /// Process error event.
    ProcessError {
        /// Gateway-assigned execution identifier.
        execution_id: String,
        /// Stable machine-readable error code.
        code: String,
        /// Human-readable error message.
        message: String,
    },
    /// Protocol-level error reported by either side.
    Error {
        /// Stable machine-readable error code.
        code: String,
        /// Human-readable error message.
        message: String,
    },
}

impl AgentMessage {
    /// Return the peer that is allowed to originate this message.
    fn origin(&self) -> ProtocolPeer {
        match self {
            Self::Ping
            | Self::Status
            | Self::ReadFile { .. }
            | Self::WriteFile { .. }
            | Self::FileInfo { .. }
            | Self::ListDir { .. }
            | Self::Exists { .. }
            | Self::CreateDir { .. }
            | Self::Remove { .. }
            | Self::CreateTempDir { .. }
            | Self::Cleanup
            | Self::StartProcess { .. }
            | Self::CancelProcess { .. } => ProtocolPeer::Host,
            Self::Pong
            | Self::StatusResult { .. }
            | Self::ReadFileResult { .. }
            | Self::WriteFileResult
            | Self::FileInfoResult { .. }
            | Self::ListDirResult { .. }
            | Self::ExistsResult { .. }
            | Self::CreateDirResult
            | Self::RemoveResult
            | Self::CreateTempDirResult { .. }
            | Self::CleanupResult
            | Self::ProcessStarted { .. }
            | Self::CancelProcessResult { .. }
            | Self::ProcessStdout { .. }
            | Self::ProcessStderr { .. }
            | Self::ProcessExit { .. }
            | Self::ProcessTimeout { .. }
            | Self::ProcessError { .. }
            | Self::Error { .. } => ProtocolPeer::Guest,
        }
    }

    /// Validate protocol invariants that do not depend on transport state.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Ping
            | Self::Pong
            | Self::Status
            | Self::WriteFileResult
            | Self::CreateDirResult
            | Self::RemoveResult
            | Self::Cleanup
            | Self::CleanupResult => Ok(()),
            Self::StatusResult {
                agent_version,
                active_processes: _,
            } => {
                validate_text(agent_version, "agent version")?;
                Ok(())
            }
            Self::ReadFile { path, .. } | Self::FileInfo { path } | Self::ListDir { path } => {
                validate_workspace_path(path)
            }
            Self::ReadFileResult { content } => validate_file_content(content),
            Self::WriteFile { path, content, .. } => {
                validate_workspace_path(path)?;
                validate_file_content(content)
            }
            Self::FileInfoResult { entry } => validate_file_entry(entry),
            Self::ListDirResult { entries } => {
                for entry in entries {
                    validate_file_entry(entry)?;
                }
                Ok(())
            }
            Self::Exists { path } | Self::CreateDir { path, .. } | Self::Remove { path, .. } => {
                validate_workspace_path(path)
            }
            Self::ExistsResult { .. } => Ok(()),
            Self::CreateTempDir { prefix } => {
                if prefix.is_empty()
                    || prefix.contains('/')
                    || prefix.contains('\\')
                    || prefix.contains('\0')
                    || prefix == "."
                    || prefix == ".."
                {
                    return Err(ProtocolError::InvalidMessage(
                        "temporary directory prefix must be a single path component",
                    ));
                }
                Ok(())
            }
            Self::CreateTempDirResult { path } => validate_workspace_path(path),
            Self::StartProcess {
                command,
                cwd,
                env,
                timeout_ms,
                max_output_bytes,
            } => {
                validate_text(command, "process command")?;
                if let Some(cwd) = cwd {
                    validate_workspace_path(cwd)?;
                }
                for (key, value) in env {
                    validate_text(key, "environment variable name")?;
                    if value.contains('\0') {
                        return Err(ProtocolError::InvalidMessage(
                            "environment variable value contains a NUL byte",
                        ));
                    }
                }
                if matches!(timeout_ms, Some(0)) {
                    return Err(ProtocolError::InvalidMessage(
                        "process timeout must be greater than zero",
                    ));
                }
                if *max_output_bytes == 0 {
                    return Err(ProtocolError::InvalidMessage(
                        "process output limit must be greater than zero",
                    ));
                }
                if *max_output_bytes > MAX_PROCESS_OUTPUT_BYTES {
                    return Err(ProtocolError::InvalidMessage(
                        "process output limit exceeds the protocol maximum",
                    ));
                }
                Ok(())
            }
            Self::ProcessStarted { execution_id, pid } => {
                validate_execution_id(execution_id)?;
                if *pid == 0 {
                    return Err(ProtocolError::InvalidMessage("process pid must be nonzero"));
                }
                Ok(())
            }
            Self::CancelProcess { execution_id, .. }
            | Self::ProcessStdout { execution_id, .. }
            | Self::ProcessStderr { execution_id, .. }
            | Self::ProcessError { execution_id, .. } => validate_execution_id(execution_id),
            Self::CancelProcessResult { .. } => Ok(()),
            Self::ProcessExit { execution_id, .. } | Self::ProcessTimeout { execution_id, .. } => {
                validate_execution_id(execution_id)
            }
            Self::Error { code, message } => {
                validate_text(code, "error code")?;
                validate_text(message, "error message")
            }
        }
    }
}

/// A decoded protocol frame with its transport-level request identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Identifier used to correlate requests and responses.
    request_id: u64,
    /// Validated protocol message.
    message: AgentMessage,
}

impl Frame {
    /// Create a frame after validating `message`.
    pub fn new(
        peer: ProtocolPeer,
        request_id: u64,
        message: AgentMessage,
    ) -> Result<Self, ProtocolError> {
        if message.origin() != peer {
            return Err(ProtocolError::InvalidMessage(
                "message is not valid for this protocol peer",
            ));
        }
        message.validate()?;
        Ok(Self {
            request_id,
            message,
        })
    }

    /// Return the request ID used for correlation.
    pub fn request_id(&self) -> u64 {
        self.request_id
    }

    /// Return the protocol peer that originated this frame.
    pub fn peer(&self) -> ProtocolPeer {
        self.message.origin()
    }

    /// Return the validated message.
    pub fn message(&self) -> &AgentMessage {
        &self.message
    }

    /// Encode a bounded frame using the protocol header and compact JSON body.
    pub fn encode(&self, max_payload_bytes: usize) -> Result<Vec<u8>, ProtocolError> {
        validate_payload_limit(max_payload_bytes)?;
        self.message.validate()?;
        let payload = serde_json::to_vec(&self.message)?;
        if payload.len() > max_payload_bytes {
            return Err(ProtocolError::PayloadTooLarge {
                actual: payload.len(),
                limit: max_payload_bytes,
            });
        }
        let payload_len =
            u32::try_from(payload.len()).map_err(|_| ProtocolError::PayloadTooLarge {
                actual: payload.len(),
                limit: max_payload_bytes,
            })?;
        let mut bytes = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
        bytes.extend_from_slice(&MAGIC);
        bytes.push(PROTOCOL_VERSION);
        bytes.extend_from_slice(&[0, 0, 0]);
        bytes.extend_from_slice(&payload_len.to_be_bytes());
        bytes.extend_from_slice(&self.request_id.to_be_bytes());
        bytes.extend_from_slice(&payload);
        Ok(bytes)
    }
}

/// Incremental, bounded decoder for guest-agent frames.
#[derive(Debug)]
pub struct FrameDecoder {
    expected_peer: ProtocolPeer,
    max_payload_bytes: usize,
    header: [u8; FRAME_HEADER_LEN],
    header_len: usize,
    payload: Option<Vec<u8>>,
    payload_len: usize,
}

impl FrameDecoder {
    /// Create a decoder for messages originating from `expected_peer`.
    pub fn new(
        expected_peer: ProtocolPeer,
        max_payload_bytes: usize,
    ) -> Result<Self, ProtocolError> {
        validate_payload_limit(max_payload_bytes)?;
        Ok(Self {
            expected_peer,
            max_payload_bytes,
            header: [0; FRAME_HEADER_LEN],
            header_len: 0,
            payload: None,
            payload_len: 0,
        })
    }

    /// Decode all complete frames in `input`, retaining incomplete state.
    ///
    /// Internal buffering never exceeds one frame header plus the configured
    /// maximum payload. Callers should still pass bounded transport read
    /// chunks because the returned vector holds every frame decoded from the
    /// supplied chunk.
    pub fn decode(&mut self, input: &[u8], frames: &mut Vec<Frame>) -> Result<(), ProtocolError> {
        let mut input = input;
        while !input.is_empty() {
            if self.header_len < FRAME_HEADER_LEN {
                let needed = FRAME_HEADER_LEN - self.header_len;
                let copied = needed.min(input.len());
                self.header[self.header_len..self.header_len + copied]
                    .copy_from_slice(&input[..copied]);
                self.header_len += copied;
                input = &input[copied..];
                if self.header_len < FRAME_HEADER_LEN {
                    break;
                }
                self.start_payload()?;
            }

            let payload = self
                .payload
                .as_mut()
                .expect("payload buffer is allocated after header");
            let needed = self.payload_len - payload.len();
            let copied = needed.min(input.len());
            payload.extend_from_slice(&input[..copied]);
            input = &input[copied..];
            if payload.len() == self.payload_len {
                self.finish_payload(frames)?;
            }
        }
        Ok(())
    }

    fn start_payload(&mut self) -> Result<(), ProtocolError> {
        if self.header[..4] != MAGIC {
            return Err(ProtocolError::InvalidHeader("magic mismatch"));
        }
        if self.header[4] != PROTOCOL_VERSION {
            return Err(ProtocolError::InvalidHeader("version mismatch"));
        }
        if self.header[5..8] != [0, 0, 0] {
            return Err(ProtocolError::InvalidHeader("reserved bytes must be zero"));
        }
        let payload_len = u32::from_be_bytes([
            self.header[8],
            self.header[9],
            self.header[10],
            self.header[11],
        ]) as usize;
        if payload_len == 0 {
            return Err(ProtocolError::InvalidHeader(
                "payload length must be nonzero",
            ));
        }
        if payload_len > self.max_payload_bytes {
            return Err(ProtocolError::PayloadTooLarge {
                actual: payload_len,
                limit: self.max_payload_bytes,
            });
        }
        self.payload = Some(Vec::with_capacity(payload_len));
        self.payload_len = payload_len;
        Ok(())
    }

    fn finish_payload(&mut self, frames: &mut Vec<Frame>) -> Result<(), ProtocolError> {
        let payload = self
            .payload
            .take()
            .expect("payload buffer is present while completing frame");
        let request_id = u64::from_be_bytes(
            self.header[12..20]
                .try_into()
                .expect("the frame header stores exactly eight request-id bytes"),
        );
        let message: AgentMessage = serde_json::from_slice(&payload)?;
        message.validate()?;
        if message.origin() != self.expected_peer {
            return Err(ProtocolError::InvalidMessage(
                "message is not valid for this protocol peer",
            ));
        }
        frames.push(Frame {
            request_id,
            message,
        });
        self.header = [0; FRAME_HEADER_LEN];
        self.header_len = 0;
        self.payload_len = 0;
        Ok(())
    }
}

fn validate_payload_limit(max_payload_bytes: usize) -> Result<(), ProtocolError> {
    if max_payload_bytes == 0 || max_payload_bytes > u32::MAX as usize {
        return Err(ProtocolError::InvalidMessage(
            "frame payload limit must be between 1 and u32::MAX",
        ));
    }
    Ok(())
}

fn validate_text(value: &str, field: &'static str) -> Result<(), ProtocolError> {
    if value.is_empty() || value.contains('\0') {
        return Err(ProtocolError::InvalidMessage(field));
    }
    Ok(())
}

fn validate_workspace_path(path: &str) -> Result<(), ProtocolError> {
    if path != "/workspace"
        && (!path.starts_with("/workspace/")
            || path.ends_with('/')
            || path.contains("//")
            || path.contains('\0')
            || path.contains('\\')
            || path
                .split('/')
                .any(|component| component == "." || component == ".."))
    {
        return Err(ProtocolError::InvalidMessage(
            "path must be /workspace or a normalized subpath",
        ));
    }
    Ok(())
}

fn validate_file_content(content: &FileContent) -> Result<(), ProtocolError> {
    match content {
        FileContent::Text { value } => {
            if value.contains('\0') {
                return Err(ProtocolError::InvalidMessage(
                    "text file content contains a NUL byte",
                ));
            }
            Ok(())
        }
        FileContent::Binary { base64 } => {
            base64::engine::general_purpose::STANDARD
                .decode(base64)
                .map_err(|_| {
                    ProtocolError::InvalidMessage("binary file content is not valid base64")
                })?;
            Ok(())
        }
    }
}

fn validate_file_entry(entry: &FileEntry) -> Result<(), ProtocolError> {
    validate_workspace_path(&entry.path)
}

fn validate_execution_id(execution_id: &str) -> Result<(), ProtocolError> {
    if execution_id.is_empty() || execution_id.len() > 128 || execution_id.contains('\0') {
        return Err(ProtocolError::InvalidMessage(
            "execution id must contain 1 to 128 non-NUL bytes",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_bytes(request_id: u64, payload: &[u8]) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.push(PROTOCOL_VERSION);
        bytes.extend_from_slice(&[0, 0, 0]);
        bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&request_id.to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn frame_round_trips_and_preserves_request_id() {
        let frame = Frame::new(
            ProtocolPeer::Host,
            42,
            AgentMessage::StartProcess {
                command: "printf ok".to_owned(),
                cwd: Some("/workspace".to_owned()),
                env: BTreeMap::from([("PATH".to_owned(), "/usr/bin".to_owned())]),
                timeout_ms: Some(1_000),
                max_output_bytes: 1024,
            },
        )
        .expect("valid frame");
        let bytes = frame.encode(DEFAULT_MAX_PAYLOAD_BYTES).expect("encoded");
        let mut decoder =
            FrameDecoder::new(ProtocolPeer::Host, DEFAULT_MAX_PAYLOAD_BYTES).expect("decoder");
        let mut frames = Vec::new();
        decoder.decode(&bytes, &mut frames).expect("decoded");
        assert_eq!(frames, vec![frame]);
    }

    #[test]
    fn decoder_handles_partial_and_concatenated_frames() {
        let first = Frame::new(ProtocolPeer::Host, 1, AgentMessage::Ping)
            .and_then(|frame| frame.encode(1024))
            .expect("first frame");
        let second = Frame::new(ProtocolPeer::Host, 2, AgentMessage::Status)
            .and_then(|frame| frame.encode(1024))
            .expect("second frame");
        let mut wire = first.clone();
        wire.extend_from_slice(&second);
        let mut decoder = FrameDecoder::new(ProtocolPeer::Host, 1024).expect("decoder");
        let mut frames = Vec::new();
        decoder.decode(&wire[..7], &mut frames).expect("partial");
        assert!(frames.is_empty());
        decoder.decode(&wire[7..], &mut frames).expect("remainder");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].request_id(), 1);
        assert_eq!(frames[1].request_id(), 2);
    }

    #[test]
    fn process_exit_carries_metadata_without_duplicating_output() {
        let frame = Frame::new(
            ProtocolPeer::Guest,
            7,
            AgentMessage::ProcessExit {
                execution_id: "exec-1".to_owned(),
                result: ProcessResult {
                    exit_code: 0,
                    stdout_truncated_bytes: 12,
                    stderr_truncated_bytes: 0,
                },
            },
        )
        .expect("exit frame");
        let bytes = frame.encode(256).expect("encoded");
        assert!(
            !bytes
                .windows(b"\"stdout\"".len())
                .any(|window| window == b"\"stdout\"")
        );
        let mut decoder = FrameDecoder::new(ProtocolPeer::Guest, 256).expect("decoder");
        let mut frames = Vec::new();
        decoder.decode(&bytes, &mut frames).expect("decoded");
        assert_eq!(frames, vec![frame]);
    }

    #[test]
    fn decoder_rejects_oversized_header_before_allocation() {
        let mut bytes = MAGIC.to_vec();
        bytes.push(PROTOCOL_VERSION);
        bytes.extend_from_slice(&[0, 0, 0]);
        bytes.extend_from_slice(&65u32.to_be_bytes());
        bytes.extend_from_slice(&1u64.to_be_bytes());
        let mut decoder = FrameDecoder::new(ProtocolPeer::Host, 64).expect("decoder");
        let mut frames = Vec::new();
        let error = decoder
            .decode(&bytes, &mut frames)
            .expect_err("oversized frame");
        assert!(matches!(
            error,
            ProtocolError::PayloadTooLarge {
                actual: 65,
                limit: 64
            }
        ));
    }

    #[test]
    fn decoder_rejects_invalid_header_and_unknown_message() {
        let mut bad_magic = frame_bytes(1, br#"{"type":"ping"}"#);
        bad_magic[0] = b'X';
        let mut decoder = FrameDecoder::new(ProtocolPeer::Host, 128).expect("decoder");
        let mut frames = Vec::new();
        assert!(matches!(
            decoder.decode(&bad_magic, &mut frames),
            Err(ProtocolError::InvalidHeader(_))
        ));

        let unknown = frame_bytes(1, br#"{"type":"unexpected"}"#);
        let mut decoder = FrameDecoder::new(ProtocolPeer::Host, 128).expect("decoder");
        let mut frames = Vec::new();
        assert!(matches!(
            decoder.decode(&unknown, &mut frames),
            Err(ProtocolError::InvalidBody(_))
        ));
    }

    #[test]
    fn decoder_rejects_unknown_fields() {
        let payload = br#"{"type":"ping","extra":true}"#;
        let wire = frame_bytes(1, payload);
        let mut decoder = FrameDecoder::new(ProtocolPeer::Host, 128).expect("decoder");
        let mut frames = Vec::new();
        assert!(matches!(
            decoder.decode(&wire, &mut frames),
            Err(ProtocolError::InvalidBody(_))
        ));
    }

    #[test]
    fn decoder_rejects_semantically_invalid_paths() {
        for path in [
            "",
            "/workspace/",
            "/workspace/./escape",
            "/workspace/../escape",
            "/workspace\\escape",
            "/tmp",
        ] {
            let frame = Frame::new(
                ProtocolPeer::Host,
                1,
                AgentMessage::ReadFile {
                    path: path.to_owned(),
                    binary: false,
                },
            );
            assert!(
                matches!(frame, Err(ProtocolError::InvalidMessage(_))),
                "expected {path:?} to be rejected"
            );
        }
    }

    #[test]
    fn decoder_rejects_invalid_binary_content() {
        let frame = Frame::new(
            ProtocolPeer::Guest,
            1,
            AgentMessage::ReadFileResult {
                content: FileContent::Binary {
                    base64: "not-base64".to_owned(),
                },
            },
        );
        assert!(matches!(frame, Err(ProtocolError::InvalidMessage(_))));
    }

    #[test]
    fn decoder_rejects_zero_timeout_and_output_limit() {
        let frame = Frame::new(
            ProtocolPeer::Host,
            1,
            AgentMessage::StartProcess {
                command: "true".to_owned(),
                cwd: None,
                env: BTreeMap::new(),
                timeout_ms: Some(0),
                max_output_bytes: 1024,
            },
        );
        assert!(matches!(frame, Err(ProtocolError::InvalidMessage(_))));

        let frame = Frame::new(
            ProtocolPeer::Host,
            1,
            AgentMessage::StartProcess {
                command: "true".to_owned(),
                cwd: None,
                env: BTreeMap::new(),
                timeout_ms: None,
                max_output_bytes: 0,
            },
        );
        assert!(matches!(frame, Err(ProtocolError::InvalidMessage(_))));

        let frame = Frame::new(
            ProtocolPeer::Host,
            1,
            AgentMessage::StartProcess {
                command: "true".to_owned(),
                cwd: None,
                env: BTreeMap::new(),
                timeout_ms: None,
                max_output_bytes: MAX_PROCESS_OUTPUT_BYTES + 1,
            },
        );
        assert!(matches!(frame, Err(ProtocolError::InvalidMessage(_))));
    }

    #[test]
    fn frame_limit_must_be_valid() {
        assert!(FrameDecoder::new(ProtocolPeer::Host, 0).is_err());
        assert!(FrameDecoder::new(ProtocolPeer::Host, u32::MAX as usize + 1).is_err());
    }

    #[test]
    fn decoder_rejects_messages_from_the_wrong_peer() {
        let wire = Frame::new(ProtocolPeer::Guest, 2, AgentMessage::Pong)
            .and_then(|frame| frame.encode(128))
            .expect("guest frame");
        let mut decoder = FrameDecoder::new(ProtocolPeer::Host, 128).expect("decoder");
        let mut frames = Vec::new();
        assert!(matches!(
            decoder.decode(&wire, &mut frames),
            Err(ProtocolError::InvalidMessage(_))
        ));
    }
}
