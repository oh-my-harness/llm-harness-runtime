//! Transport-agnostic request-response dispatcher for the VM guest agent.
//!
//! This crate connects a validated host request from [`FrameStream`] to a guest
//! handler and preserves the wire request ID on every response. It intentionally
//! does not implement process execution: process operations require a process
//! manager that can emit stdout, stderr, exit, timeout, and error events after
//! the initial response.

use std::sync::Arc;

use async_trait::async_trait;
use llm_harness_vm_agent_protocol::{AgentMessage, Frame, ProtocolPeer};
use llm_harness_vm_agent_transport::{FrameStream, TransportError};

#[cfg(target_os = "linux")]
pub mod workspace;
#[cfg(target_os = "linux")]
pub use workspace::WorkspaceFileHandler;

/// Stable error code returned when this dispatcher cannot safely handle an operation.
pub const UNSUPPORTED_OPERATION_CODE: &str = "unsupported_operation";
pub const HANDLER_ERROR_CODE: &str = "handler_error";

const MAX_ERROR_MESSAGE_BYTES: usize = 4096;
const ELLIPSIS_BYTES: usize = 3;

/// A guest-handler failure that is safe to report over the protocol.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct HandlerError {
    code: &'static str,
    message: String,
}

impl HandlerError {
    /// Create an unsupported-operation error.
    pub fn unsupported(operation: &'static str) -> Self {
        Self {
            code: UNSUPPORTED_OPERATION_CODE,
            message: sanitize_error_message(format!(
                "unsupported guest agent operation: {operation}"
            )),
        }
    }

    /// Create an error for a supported operation.
    pub fn operation(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code: sanitize_error_code(code),
            message: sanitize_error_message(message.into()),
        }
    }

    /// Return the stable error code.
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// Return the bounded, protocol-safe error message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Handles one guest-agent request and returns one guest-originating response.
#[async_trait]
pub trait GuestAgentHandler: Send + Sync {
    /// Handle a validated host request.
    async fn handle(&self, request: AgentMessage) -> Result<AgentMessage, HandlerError>;
}

/// Errors produced while serving a guest-agent connection.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// The stream was not configured for the guest peer.
    #[error("guest agent service requires a Guest stream, found {peer:?}")]
    WrongPeer {
        /// The configured local peer.
        peer: ProtocolPeer,
    },
    /// The frame transport failed or received an invalid frame.
    #[error("guest agent service transport error: {0}")]
    Transport(#[from] TransportError),
    /// The handler returned a message that violates the guest protocol.
    #[error("guest agent handler returned an invalid response: {0}")]
    InvalidResponse(#[from] llm_harness_vm_agent_protocol::ProtocolError),
}

/// A shareable request-response guest-agent service.
pub struct GuestAgentService<H> {
    handler: Arc<H>,
}

impl<H> GuestAgentService<H>
where
    H: GuestAgentHandler,
{
    /// Create a service from a handler.
    pub fn new(handler: H) -> Self {
        Self {
            handler: Arc::new(handler),
        }
    }

    /// Create a service from an already shared handler.
    pub fn shared(handler: Arc<H>) -> Self {
        Self { handler }
    }

    /// Serve one transport-agnostic guest frame stream until clean close.
    ///
    /// Requests are dispatched serially. Unsupported process operations receive
    /// a protocol error with the original request ID and do not terminate the
    /// connection. A handler response with the wrong direction closes the
    /// connection because the guest peer can no longer be trusted to follow the
    /// protocol contract.
    pub async fn serve<S>(&self, mut stream: FrameStream<S>) -> Result<(), ServiceError>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        if stream.peer() != ProtocolPeer::Guest {
            return Err(ServiceError::WrongPeer {
                peer: stream.peer(),
            });
        }

        loop {
            let request = match stream.recv().await {
                Ok(request) => request,
                Err(TransportError::Closed) => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            let request_id = request.request_id();
            let request = request.into_message();
            let operation = operation_name(&request);

            let response = match request {
                AgentMessage::StartProcess { .. } | AgentMessage::CancelProcess { .. } => {
                    Err(HandlerError::unsupported(operation))
                }
                request => self.handler.handle(request).await,
            };
            let response = response.unwrap_or_else(error_response);
            let response = Frame::new(ProtocolPeer::Guest, request_id, response)?;
            stream.send(response).await?;
        }
    }
}

fn operation_name(message: &AgentMessage) -> &'static str {
    match message {
        AgentMessage::StartProcess { .. } => "start_process",
        AgentMessage::CancelProcess { .. } => "cancel_process",
        _ => "unsupported",
    }
}

fn error_response(error: HandlerError) -> AgentMessage {
    AgentMessage::Error {
        code: error.code().to_owned(),
        message: error.message().to_owned(),
    }
}

fn sanitize_error_code(code: &'static str) -> &'static str {
    if code.is_empty() || code.contains('\0') {
        HANDLER_ERROR_CODE
    } else {
        code
    }
}

fn sanitize_error_message(message: String) -> String {
    let mut message = message.replace('\0', "\\0");
    if message.is_empty() {
        return "operation failed".to_owned();
    }
    if message.len() > MAX_ERROR_MESSAGE_BYTES {
        let mut truncate_to = MAX_ERROR_MESSAGE_BYTES - ELLIPSIS_BYTES;
        while !message.is_char_boundary(truncate_to) {
            truncate_to -= 1;
        }
        message.truncate(truncate_to);
        message.push('…');
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_vm_agent_protocol::Frame;
    use tokio::io::duplex;
    use tokio::spawn;

    struct StaticHandler {
        response: Result<AgentMessage, HandlerError>,
    }

    #[async_trait]
    impl GuestAgentHandler for StaticHandler {
        async fn handle(&self, _request: AgentMessage) -> Result<AgentMessage, HandlerError> {
            self.response.clone()
        }
    }

    fn streams() -> (
        FrameStream<tokio::io::DuplexStream>,
        FrameStream<tokio::io::DuplexStream>,
    ) {
        let (host_io, guest_io) = duplex(1024);
        let host = FrameStream::with_default_limit(host_io, ProtocolPeer::Host)
            .expect("host frame stream");
        let guest = FrameStream::with_default_limit(guest_io, ProtocolPeer::Guest)
            .expect("guest frame stream");
        (host, guest)
    }

    fn host_frame(request_id: u64, message: AgentMessage) -> Frame {
        Frame::new(ProtocolPeer::Host, request_id, message).expect("valid host frame")
    }

    #[test]
    fn handler_error_is_protocol_safe() {
        let error = HandlerError::operation("", format!("bad\0{}", "x".repeat(8192)));
        assert_eq!(error.code(), HANDLER_ERROR_CODE);
        assert!(!error.message().contains('\0'));
        assert!(error.message().len() <= MAX_ERROR_MESSAGE_BYTES);
        assert!(error.message().ends_with('…'));
    }

    #[test]
    fn handler_error_truncation_is_utf8_safe() {
        let error =
            HandlerError::operation("filesystem_error", "失败".repeat(MAX_ERROR_MESSAGE_BYTES));
        assert!(error.message().len() <= MAX_ERROR_MESSAGE_BYTES);
        assert!(error.message().ends_with('…'));
    }

    #[tokio::test]
    async fn dispatches_requests_and_preserves_request_ids() {
        let (mut host, guest) = streams();
        let service = GuestAgentService::new(StaticHandler {
            response: Ok(AgentMessage::Pong),
        });
        let service_task = spawn(async move { service.serve(guest).await });

        host.send(host_frame(41, AgentMessage::Ping))
            .await
            .expect("send ping");
        let response = host.recv().await.expect("receive pong");
        assert_eq!(response.request_id(), 41);
        assert_eq!(response.into_message(), AgentMessage::Pong);

        host.close().await.expect("close host");
        drop(host);
        service_task
            .await
            .expect("service completed")
            .expect("service ok");
    }

    #[tokio::test]
    async fn process_operations_are_rejected_but_keep_connection_open() {
        let (mut host, guest) = streams();
        let service = GuestAgentService::new(StaticHandler {
            response: Ok(AgentMessage::Pong),
        });
        let service_task = spawn(async move { service.serve(guest).await });

        host.send(host_frame(
            7,
            AgentMessage::StartProcess {
                command: "true".to_owned(),
                cwd: Some("/workspace".to_owned()),
                env: Default::default(),
                timeout_ms: Some(1_000),
                max_output_bytes: 1024,
            },
        ))
        .await
        .expect("send start process");
        let response = host.recv().await.expect("receive error");
        assert_eq!(response.request_id(), 7);
        match response.into_message() {
            AgentMessage::Error { code, message } => {
                assert_eq!(code, UNSUPPORTED_OPERATION_CODE);
                assert_eq!(message, "unsupported guest agent operation: start_process");
            }
            message => panic!("unexpected response: {message:?}"),
        }

        host.send(host_frame(8, AgentMessage::Ping))
            .await
            .expect("send ping");
        let response = host.recv().await.expect("receive pong");
        assert_eq!(response.request_id(), 8);
        assert_eq!(response.into_message(), AgentMessage::Pong);

        host.close().await.expect("close host");
        drop(host);
        service_task
            .await
            .expect("service completed")
            .expect("service ok");
    }

    #[tokio::test]
    async fn handler_errors_are_reported_with_original_request_id() {
        let (mut host, guest) = streams();
        let service = GuestAgentService::new(StaticHandler {
            response: Err(HandlerError::operation("filesystem_error", "read failed")),
        });
        let service_task = spawn(async move { service.serve(guest).await });

        host.send(host_frame(99, AgentMessage::Ping))
            .await
            .expect("send ping");
        let response = host.recv().await.expect("receive error");
        assert_eq!(response.request_id(), 99);
        match response.into_message() {
            AgentMessage::Error { code, message } => {
                assert_eq!(code, "filesystem_error");
                assert_eq!(message, "read failed");
            }
            message => panic!("unexpected response: {message:?}"),
        }

        host.close().await.expect("close host");
        drop(host);
        service_task
            .await
            .expect("service completed")
            .expect("service ok");
    }

    #[tokio::test]
    async fn wrong_local_peer_fails_before_reading() {
        let service = GuestAgentService::new(StaticHandler {
            response: Ok(AgentMessage::Pong),
        });
        let (host, _guest) = streams();
        let error = service.serve(host).await.expect_err("wrong local peer");
        assert!(matches!(
            error,
            ServiceError::WrongPeer {
                peer: ProtocolPeer::Host
            }
        ));
    }

    #[tokio::test]
    async fn invalid_handler_response_closes_the_connection() {
        let (mut host, guest) = streams();
        let service = GuestAgentService::new(StaticHandler {
            response: Ok(AgentMessage::Ping),
        });
        let service_task = spawn(async move { service.serve(guest).await });

        host.send(host_frame(15, AgentMessage::Ping))
            .await
            .expect("send ping");
        let error = service_task
            .await
            .expect("service completed")
            .expect_err("handler returned host message");
        assert!(matches!(
            error,
            ServiceError::InvalidResponse(
                llm_harness_vm_agent_protocol::ProtocolError::InvalidMessage(_)
            )
        ));
        assert!(matches!(host.recv().await, Err(TransportError::Closed)));
    }
}
