//! Transport-agnostic request-response dispatcher for the VM guest agent.
//!
//! This crate connects a validated host request from [`FrameStream`] to a guest
//! handler and preserves the wire request ID on every response. Process
//! handlers can also subscribe the service to stdout, stderr, exit, timeout,
//! and error events emitted after the initial response.

use std::sync::Arc;

use async_trait::async_trait;
use llm_harness_vm_agent_protocol::{AgentMessage, Frame, ProtocolPeer};
use llm_harness_vm_agent_transport::{FrameStream, TransportError};
use tokio::sync::{mpsc, oneshot};

#[cfg(target_os = "linux")]
pub mod workspace;
#[cfg(target_os = "linux")]
pub use workspace::WorkspaceFileHandler;
#[cfg(target_os = "linux")]
pub mod process;
#[cfg(target_os = "linux")]
pub use process::ProcessManager;

/// Stable error code returned when this dispatcher cannot safely handle an operation.
pub const UNSUPPORTED_OPERATION_CODE: &str = "unsupported_operation";
pub const HANDLER_ERROR_CODE: &str = "handler_error";

const MAX_ERROR_MESSAGE_BYTES: usize = 4096;
const ELLIPSIS_BYTES: usize = 3;
const OUTBOUND_FRAME_CAPACITY: usize = 64;

/// A guest-handler failure that is safe to report over the protocol.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct HandlerError {
    code: &'static str,
    message: String,
}

/// A disconnect signal for a started process.
#[derive(Debug)]
pub struct ProcessDisconnect {
    sender: oneshot::Sender<()>,
}

impl ProcessDisconnect {
    /// Ask the process task to terminate its process and stop emitting events.
    pub fn disconnect(self) {
        let _ = self.sender.send(());
    }
}

/// The result returned by a guest handler for one host request.
#[derive(Debug)]
pub enum HandlerOutcome {
    /// A single response correlated with the request ID.
    Message(AgentMessage),
    /// A process response followed by asynchronous process events.
    ProcessStarted {
        /// The immediate `ProcessStarted` response.
        response: AgentMessage,
        /// Validated guest-originating process events.
        events: mpsc::Receiver<AgentMessage>,
        /// Connection-scoped cleanup signal for the process task.
        disconnect: ProcessDisconnect,
    },
}

impl From<AgentMessage> for HandlerOutcome {
    fn from(message: AgentMessage) -> Self {
        Self::Message(message)
    }
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
    async fn handle(&self, request: AgentMessage) -> Result<HandlerOutcome, HandlerError>;
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
    /// A service task failed to join.
    #[error("guest agent service task failed: {0}")]
    Join(String),
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
    pub async fn serve<S>(&self, stream: FrameStream<S>) -> Result<(), ServiceError>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        if stream.peer() != ProtocolPeer::Guest {
            return Err(ServiceError::WrongPeer {
                peer: stream.peer(),
            });
        }

        let (mut reader, writer) = stream.split()?;
        let (outbound_tx, mut outbound_rx) = mpsc::channel(OUTBOUND_FRAME_CAPACITY);
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
        let writer_shutdown_tx = shutdown_tx.clone();
        let writer_task = tokio::spawn(async move {
            let mut writer = writer;
            while let Some(frame) = outbound_rx.recv().await {
                if let Err(error) = writer.send(frame).await {
                    let error = ServiceError::Transport(error);
                    let _ = writer_shutdown_tx.try_send(error);
                    return Ok(());
                }
            }
            if let Err(error) = writer.close().await {
                let error = ServiceError::Transport(error);
                let _ = writer_shutdown_tx.try_send(error);
                return Ok(());
            }
            Ok::<(), ServiceError>(())
        });

        let mut event_tasks = Vec::new();
        let mut disconnects = Vec::new();
        let service_result: Result<(), ServiceError> = async {
            loop {
                let request = tokio::select! {
                    request = reader.recv() => match request {
                        Ok(request) => request,
                        Err(TransportError::Closed) => break,
                        Err(error) => return Err(error.into()),
                    },
                    error = shutdown_rx.recv() => {
                        return Err(error.unwrap_or_else(|| ServiceError::Join(
                            "process event shutdown channel closed unexpectedly".to_owned(),
                        )));
                    }
                };
                let request_id = request.request_id();
                let request = request.into_message();

                let outcome = self.handler.handle(request).await;
                match outcome {
                    Ok(HandlerOutcome::Message(response)) => {
                        let frame = Frame::new(ProtocolPeer::Guest, request_id, response)?;
                        if outbound_tx.send(frame).await.is_err() {
                            break;
                        }
                    }
                    Ok(HandlerOutcome::ProcessStarted {
                        response,
                        events,
                        disconnect,
                    }) => {
                        let frame = Frame::new(ProtocolPeer::Guest, request_id, response)?;
                        if outbound_tx.send(frame).await.is_err() {
                            disconnect.disconnect();
                            break;
                        }
                        let outbound_tx = outbound_tx.clone();
                        let shutdown_tx = shutdown_tx.clone();
                        event_tasks.push(tokio::spawn(async move {
                            let mut events = events;
                            while let Some(message) = events.recv().await {
                                let frame =
                                    match Frame::new(ProtocolPeer::Guest, request_id, message) {
                                        Ok(frame) => frame,
                                        Err(error) => {
                                            let _ = shutdown_tx
                                                .try_send(ServiceError::InvalidResponse(error));
                                            break;
                                        }
                                    };
                                if outbound_tx.send(frame).await.is_err() {
                                    break;
                                }
                            }
                        }));
                        disconnects.push(disconnect);
                    }
                    Err(error) => {
                        let response = error_response(error);
                        let frame = Frame::new(ProtocolPeer::Guest, request_id, response)?;
                        if outbound_tx.send(frame).await.is_err() {
                            break;
                        }
                    }
                }
            }
            if let Ok(error) = shutdown_rx.try_recv() {
                return Err(error);
            }
            Ok(())
        }
        .await;

        for disconnect in disconnects {
            disconnect.disconnect();
        }
        for task in event_tasks {
            task.abort();
        }
        drop(outbound_tx);
        let writer_result = writer_task
            .await
            .map_err(|error| ServiceError::Join(error.to_string()))
            .and_then(|result| result);

        service_result?;
        writer_result
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
    use std::io;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf, duplex};
    use tokio::spawn;

    struct StaticHandler {
        response: Result<AgentMessage, HandlerError>,
    }

    struct EventfulProcessHandler;

    struct InvalidEventProcessHandler {
        disconnected: Arc<AtomicBool>,
    }

    struct FailingWriteStream {
        stream: DuplexStream,
        fail_next_write: AtomicBool,
    }

    impl FailingWriteStream {
        fn new(stream: DuplexStream) -> Self {
            Self {
                stream,
                fail_next_write: AtomicBool::new(true),
            }
        }
    }

    impl AsyncRead for FailingWriteStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.stream).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for FailingWriteStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.fail_next_write.swap(false, Ordering::SeqCst) {
                return Poll::Ready(Err(io::Error::other("test write failure")));
            }
            Pin::new(&mut self.stream).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.stream).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.stream).poll_shutdown(cx)
        }
    }

    #[async_trait]
    impl GuestAgentHandler for StaticHandler {
        async fn handle(&self, _request: AgentMessage) -> Result<HandlerOutcome, HandlerError> {
            Ok(HandlerOutcome::Message(self.response.clone()?))
        }
    }

    #[async_trait]
    impl GuestAgentHandler for EventfulProcessHandler {
        async fn handle(&self, request: AgentMessage) -> Result<HandlerOutcome, HandlerError> {
            match request {
                AgentMessage::StartProcess { .. } => {
                    let (event_tx, event_rx) = tokio::sync::mpsc::channel(8);
                    let (disconnect_tx, _disconnect_rx) = tokio::sync::oneshot::channel();
                    tokio::spawn(async move {
                        event_tx
                            .send(AgentMessage::ProcessStdout {
                                execution_id: "test-execution".to_owned(),
                                chunk: "stdout".to_owned(),
                            })
                            .await
                            .expect("send stdout");
                        event_tx
                            .send(AgentMessage::ProcessExit {
                                execution_id: "test-execution".to_owned(),
                                result: Default::default(),
                            })
                            .await
                            .expect("send exit");
                    });
                    Ok(HandlerOutcome::ProcessStarted {
                        response: AgentMessage::ProcessStarted {
                            execution_id: "test-execution".to_owned(),
                            pid: 123,
                        },
                        events: event_rx,
                        disconnect: ProcessDisconnect {
                            sender: disconnect_tx,
                        },
                    })
                }
                AgentMessage::Ping => Ok(HandlerOutcome::Message(AgentMessage::Pong)),
                _ => Err(HandlerError::unsupported("operation")),
            }
        }
    }

    #[async_trait]
    impl GuestAgentHandler for InvalidEventProcessHandler {
        async fn handle(&self, request: AgentMessage) -> Result<HandlerOutcome, HandlerError> {
            match request {
                AgentMessage::StartProcess { .. } => {
                    let (event_tx, event_rx) = tokio::sync::mpsc::channel(8);
                    let (disconnect_tx, disconnect_rx) = tokio::sync::oneshot::channel();
                    let observer = Arc::clone(&self.disconnected);
                    tokio::spawn(async move {
                        event_tx
                            .send(AgentMessage::Ping)
                            .await
                            .expect("send invalid process event");
                        let _ = disconnect_rx.await;
                        observer.store(true, Ordering::SeqCst);
                    });
                    Ok(HandlerOutcome::ProcessStarted {
                        response: AgentMessage::ProcessStarted {
                            execution_id: "test-execution".to_owned(),
                            pid: 123,
                        },
                        events: event_rx,
                        disconnect: ProcessDisconnect {
                            sender: disconnect_tx,
                        },
                    })
                }
                _ => Err(HandlerError::unsupported("operation")),
            }
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
    async fn process_events_are_multiplexed_with_requests() {
        let (mut host, guest) = streams();
        let service = GuestAgentService::new(EventfulProcessHandler);
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
        assert_eq!(
            response.into_message(),
            AgentMessage::ProcessStarted {
                execution_id: "test-execution".to_owned(),
                pid: 123,
            }
        );

        let response = host.recv().await.expect("receive stdout");
        assert_eq!(
            response.into_message(),
            AgentMessage::ProcessStdout {
                execution_id: "test-execution".to_owned(),
                chunk: "stdout".to_owned(),
            }
        );

        host.send(host_frame(8, AgentMessage::Ping))
            .await
            .expect("send ping");
        let mut exit_seen = false;
        loop {
            let response = host.recv().await.expect("receive response after ping");
            if response.request_id() == 8 {
                assert_eq!(response.into_message(), AgentMessage::Pong);
                break;
            }
            assert_eq!(response.request_id(), 7);
            if matches!(response.into_message(), AgentMessage::ProcessExit { .. }) {
                exit_seen = true;
            }
        }
        assert!(exit_seen, "process exit event was received");

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

    #[tokio::test]
    async fn invalid_process_event_closes_the_connection_and_disconnects() {
        let (mut host, guest) = streams();
        let disconnected = Arc::new(AtomicBool::new(false));
        let service = GuestAgentService::new(InvalidEventProcessHandler {
            disconnected: Arc::clone(&disconnected),
        });
        let service_task = spawn(async move { service.serve(guest).await });

        host.send(host_frame(
            16,
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
        let response = host.recv().await.expect("receive started");
        assert_eq!(response.request_id(), 16);
        assert!(matches!(
            response.into_message(),
            AgentMessage::ProcessStarted { .. }
        ));

        let error = service_task
            .await
            .expect("service completed")
            .expect_err("process emitted a host message");
        assert!(matches!(
            error,
            ServiceError::InvalidResponse(
                llm_harness_vm_agent_protocol::ProtocolError::InvalidMessage(_)
            )
        ));
        assert!(matches!(host.recv().await, Err(TransportError::Closed)));
        assert!(disconnected.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn writer_failure_stops_the_service_and_disconnects_processes() {
        let (host_io, guest_io) = duplex(1024);
        let mut host =
            FrameStream::with_default_limit(host_io, ProtocolPeer::Host).expect("host stream");
        let guest =
            FrameStream::with_default_limit(FailingWriteStream::new(guest_io), ProtocolPeer::Guest)
                .expect("guest stream");
        let disconnected = Arc::new(AtomicBool::new(false));
        let service = GuestAgentService::new(InvalidEventProcessHandler {
            disconnected: Arc::clone(&disconnected),
        });
        let service_task = spawn(async move { service.serve(guest).await });

        host.send(host_frame(
            17,
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

        let error = service_task
            .await
            .expect("service completed")
            .expect_err("writer failed");
        assert!(matches!(
            error,
            ServiceError::Transport(TransportError::Io(_))
        ));
        assert!(matches!(host.recv().await, Err(TransportError::Closed)));
        assert!(disconnected.load(Ordering::SeqCst));
    }
}
