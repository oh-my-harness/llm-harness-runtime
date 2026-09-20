//! Host-side client for the VM guest-agent protocol.
//!
//! This client multiplexes request/response traffic with asynchronous process
//! events on one bounded frame stream.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use llm_harness_vm_agent_protocol::{AgentMessage, Frame, ProtocolPeer};
#[cfg(target_os = "linux")]
use llm_harness_vm_agent_transport::vsock::{VsockAddress, VsockStream};
use llm_harness_vm_agent_transport::{FrameStream, TransportError};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const REQUEST_CHANNEL_CAPACITY: usize = 64;
const EVENT_CHANNEL_CAPACITY: usize = 128;

/// Errors produced by the host-side guest-agent client.
#[derive(Debug, Clone, thiserror::Error)]
pub enum GuestAgentClientError {
    /// The guest-agent connection closed before a request completed.
    #[error("guest agent connection closed")]
    ConnectionClosed,
    /// The guest-agent protocol was violated.
    #[error("guest agent protocol error: {0}")]
    Protocol(String),
    /// The guest agent rejected a request.
    #[error("guest agent request failed: {0}")]
    Request(String),
    /// The background connection task failed to join.
    #[error("guest agent client task failed: {0}")]
    Join(String),
}

/// A running guest process and its event stream.
#[derive(Debug)]
pub struct GuestProcessHandle {
    /// Guest-agent execution identifier.
    pub execution_id: String,
    /// Guest process identifier.
    pub pid: u32,
    /// Stream of stdout, stderr, exit, timeout, and error events.
    pub events: mpsc::Receiver<AgentMessage>,
}

enum PendingRequest {
    Response(oneshot::Sender<Result<AgentMessage, GuestAgentClientError>>),
    Process {
        response: Option<oneshot::Sender<Result<AgentMessage, GuestAgentClientError>>>,
        events: Option<mpsc::Sender<AgentMessage>>,
    },
}

/// A host-side client for one guest-agent connection.
pub struct GuestAgentClient {
    request_tx: mpsc::Sender<Frame>,
    pending: Arc<Mutex<HashMap<u64, PendingRequest>>>,
    next_request_id: AtomicU64,
    actor: JoinHandle<Result<(), GuestAgentClientError>>,
}

impl GuestAgentClient {
    /// Connect to a Linux AF_VSOCK guest agent.
    #[cfg(target_os = "linux")]
    pub async fn connect_vsock(address: VsockAddress) -> Result<Self, GuestAgentClientError> {
        let stream = VsockStream::connect(address)
            .await
            .map_err(|error| GuestAgentClientError::Protocol(error.to_string()))?;
        Self::new(stream)
    }

    /// Create a client over any duplex byte stream.
    pub fn new<S>(stream: S) -> Result<Self, GuestAgentClientError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let frame_stream =
            FrameStream::with_default_limit(stream, ProtocolPeer::Host).map_err(transport_error)?;
        Self::from_frame_stream(frame_stream)
    }

    fn from_frame_stream<S>(frame_stream: FrameStream<S>) -> Result<Self, GuestAgentClientError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (request_tx, request_rx) = mpsc::channel(REQUEST_CHANNEL_CAPACITY);
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let actor = tokio::spawn(run_client(frame_stream, request_rx, Arc::clone(&pending)));
        Ok(Self {
            request_tx,
            pending,
            next_request_id: AtomicU64::new(1),
            actor,
        })
    }

    /// Send one request and await its correlated response.
    pub async fn request(
        &self,
        message: AgentMessage,
    ) -> Result<AgentMessage, GuestAgentClientError> {
        if matches!(message, AgentMessage::StartProcess { .. }) {
            return Err(GuestAgentClientError::Protocol(
                "use start_process for process requests".to_owned(),
            ));
        }
        let request_id = self.allocate_request_id();
        let (response_tx, response_rx) = oneshot::channel();
        self.insert_pending(request_id, PendingRequest::Response(response_tx));

        let frame = Frame::new(ProtocolPeer::Host, request_id, message)
            .inspect_err(|_| self.remove_pending(request_id))
            .map_err(|error| GuestAgentClientError::Protocol(error.to_string()))?;
        if self.request_tx.send(frame).await.is_err() {
            self.remove_pending(request_id);
            return Err(GuestAgentClientError::ConnectionClosed);
        }

        match response_rx.await {
            Ok(result) => result,
            Err(_) => Err(GuestAgentClientError::ConnectionClosed),
        }
    }

    /// Check guest-agent liveness.
    pub async fn ping(&self) -> Result<(), GuestAgentClientError> {
        match self.request(AgentMessage::Ping).await? {
            AgentMessage::Pong => Ok(()),
            AgentMessage::Error { code, message } => Err(request_error(code, message)),
            other => Err(unexpected_response("ping", other)),
        }
    }

    /// Request guest-agent status.
    pub async fn status(&self) -> Result<AgentMessage, GuestAgentClientError> {
        let response = self.request(AgentMessage::Status).await?;
        match response {
            AgentMessage::StatusResult { .. } => Ok(response),
            AgentMessage::Error { code, message } => Err(request_error(code, message)),
            other => Err(unexpected_response("status", other)),
        }
    }

    /// Request guest-agent cleanup.
    pub async fn cleanup(&self) -> Result<(), GuestAgentClientError> {
        match self.request(AgentMessage::Cleanup).await? {
            AgentMessage::CleanupResult => Ok(()),
            AgentMessage::Error { code, message } => Err(request_error(code, message)),
            other => Err(unexpected_response("cleanup", other)),
        }
    }

    /// Start a process and return its event stream.
    pub async fn start_process(
        &self,
        command: String,
        cwd: Option<String>,
        env: BTreeMap<String, String>,
        timeout_ms: Option<u64>,
        max_output_bytes: u64,
    ) -> Result<GuestProcessHandle, GuestAgentClientError> {
        let request_id = self.allocate_request_id();
        let (response_tx, response_rx) = oneshot::channel();
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        self.insert_pending(
            request_id,
            PendingRequest::Process {
                response: Some(response_tx),
                events: Some(event_tx),
            },
        );

        let message = AgentMessage::StartProcess {
            command,
            cwd,
            env,
            timeout_ms,
            max_output_bytes,
        };
        let frame = Frame::new(ProtocolPeer::Host, request_id, message)
            .inspect_err(|_| self.remove_pending(request_id))
            .map_err(|error| GuestAgentClientError::Protocol(error.to_string()))?;
        if self.request_tx.send(frame).await.is_err() {
            self.remove_pending(request_id);
            return Err(GuestAgentClientError::ConnectionClosed);
        }

        let response = match response_rx.await {
            Ok(result) => result?,
            Err(_) => return Err(GuestAgentClientError::ConnectionClosed),
        };
        match response {
            AgentMessage::ProcessStarted { execution_id, pid } => Ok(GuestProcessHandle {
                execution_id,
                pid,
                events: event_rx,
            }),
            AgentMessage::Error { code, message } => Err(request_error(code, message)),
            other => Err(unexpected_response("start process", other)),
        }
    }

    /// Cancel a previously started process.
    pub async fn cancel_process(
        &self,
        execution_id: &str,
        force: bool,
    ) -> Result<bool, GuestAgentClientError> {
        let response = self
            .request(AgentMessage::CancelProcess {
                execution_id: execution_id.to_owned(),
                force,
            })
            .await?;
        match response {
            AgentMessage::CancelProcessResult { cancelled } => Ok(cancelled),
            AgentMessage::Error { code, message } => Err(request_error(code, message)),
            other => Err(unexpected_response("cancel process", other)),
        }
    }

    /// Wait for the background connection task to finish.
    pub async fn join(self) -> Result<(), GuestAgentClientError> {
        self.actor
            .await
            .map_err(|error| GuestAgentClientError::Join(error.to_string()))?
    }

    fn allocate_request_id(&self) -> u64 {
        self.next_request_id.fetch_add(1, Ordering::SeqCst)
    }

    fn insert_pending(&self, request_id: u64, pending: PendingRequest) {
        self.pending
            .lock()
            .expect("guest agent pending map poisoned")
            .insert(request_id, pending);
    }

    fn remove_pending(&self, request_id: u64) {
        self.pending
            .lock()
            .expect("guest agent pending map poisoned")
            .remove(&request_id);
    }
}

async fn run_client<S>(
    frame_stream: FrameStream<S>,
    mut request_rx: mpsc::Receiver<Frame>,
    pending: Arc<Mutex<HashMap<u64, PendingRequest>>>,
) -> Result<(), GuestAgentClientError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = frame_stream
        .split()
        .map_err(|error| GuestAgentClientError::Protocol(error.to_string()))?;

    let result = loop {
        tokio::select! {
            response = reader.recv() => {
                match response {
                    Ok(frame) => {
                        if let Err(error) = route_frame(frame, &pending).await {
                            break Err(error);
                        }
                    }
                    Err(TransportError::Closed) => {
                        let _ = writer.close().await;
                        break Ok(());
                    }
                    Err(error) => break Err(transport_error(error)),
                }
            }
            request = request_rx.recv() => {
                match request {
                    Some(frame) => {
                        if writer.send(frame).await.is_err() {
                            break Err(GuestAgentClientError::ConnectionClosed);
                        }
                    }
                    None => {
                        let _ = writer.close().await;
                        break Ok(());
                    }
                }
            }
        }
    };

    let final_error = match &result {
        Ok(()) => GuestAgentClientError::ConnectionClosed,
        Err(error) => error.clone(),
    };
    fail_pending(&pending, final_error).await;
    result
}

async fn route_frame(
    frame: Frame,
    pending: &Arc<Mutex<HashMap<u64, PendingRequest>>>,
) -> Result<(), GuestAgentClientError> {
    let request_id = frame.request_id();
    let message = frame.into_message();
    let route = {
        let mut pending = pending.lock().expect("guest agent pending map poisoned");
        pending.remove(&request_id).ok_or_else(|| {
            GuestAgentClientError::Protocol(format!(
                "guest agent response used unknown request id {request_id}"
            ))
        })?
    };

    match route {
        PendingRequest::Response(sender) => {
            let _ = sender.send(response_result(message));
        }
        PendingRequest::Process {
            mut response,
            mut events,
        } => match message {
            AgentMessage::ProcessStarted { .. } => {
                if let Some(sender) = response.take() {
                    let _ = sender.send(response_result(message));
                }
                pending
                    .lock()
                    .expect("guest agent pending map poisoned")
                    .insert(
                        request_id,
                        PendingRequest::Process {
                            response: None,
                            events,
                        },
                    );
            }
            AgentMessage::ProcessStdout { .. }
            | AgentMessage::ProcessStderr { .. }
            | AgentMessage::ProcessExit { .. }
            | AgentMessage::ProcessTimeout { .. }
            | AgentMessage::ProcessError { .. } => {
                let terminal = matches!(
                    message,
                    AgentMessage::ProcessExit { .. }
                        | AgentMessage::ProcessTimeout { .. }
                        | AgentMessage::ProcessError { .. }
                );
                if let Some(event_tx) = events.as_mut()
                    && event_tx.send(message).await.is_err()
                {
                    events = None;
                }
                if !terminal {
                    pending
                        .lock()
                        .expect("guest agent pending map poisoned")
                        .insert(
                            request_id,
                            PendingRequest::Process {
                                response: None,
                                events,
                            },
                        );
                }
            }
            AgentMessage::Error { code, message } => {
                if let Some(sender) = response.take() {
                    let _ = sender.send(Err(request_error(code, message)));
                } else if let Some(event_tx) = events.as_mut() {
                    let _ = event_tx.send(AgentMessage::Error { code, message }).await;
                }
            }
            other => {
                return Err(unexpected_response("process event", other));
            }
        },
    }
    Ok(())
}

async fn fail_pending(
    pending: &Arc<Mutex<HashMap<u64, PendingRequest>>>,
    error: GuestAgentClientError,
) {
    let routes: Vec<PendingRequest> = {
        let mut pending = pending.lock().expect("guest agent pending map poisoned");
        pending.drain().map(|(_, route)| route).collect()
    };

    for route in routes {
        match route {
            PendingRequest::Response(sender) => {
                let _ = sender.send(Err(error.clone()));
            }
            PendingRequest::Process { response, events } => {
                if let Some(sender) = response {
                    let _ = sender.send(Err(error.clone()));
                }
                if let Some(event_tx) = events {
                    let _ = event_tx
                        .send(AgentMessage::Error {
                            code: "connection_closed".to_owned(),
                            message: "guest agent connection closed".to_owned(),
                        })
                        .await;
                }
            }
        }
    }
}

fn response_result(message: AgentMessage) -> Result<AgentMessage, GuestAgentClientError> {
    match message {
        AgentMessage::Error { code, message } => Err(request_error(code, message)),
        message => Ok(message),
    }
}

fn request_error(code: String, message: String) -> GuestAgentClientError {
    GuestAgentClientError::Request(format!("{code}: {message}"))
}

fn unexpected_response(operation: &str, message: AgentMessage) -> GuestAgentClientError {
    GuestAgentClientError::Protocol(format!(
        "unexpected guest agent response for {operation}: {message:?}"
    ))
}

fn transport_error(error: TransportError) -> GuestAgentClientError {
    GuestAgentClientError::Protocol(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_vm_agent_protocol::ProcessResult;
    use tokio::io::duplex;
    use tokio::spawn;

    fn guest_frame(request_id: u64, message: AgentMessage) -> Frame {
        Frame::new(ProtocolPeer::Guest, request_id, message).expect("guest frame")
    }

    #[tokio::test]
    async fn ping_request_preserves_request_id_and_reports_errors() {
        let (host_io, guest_io) = duplex(4096);
        let client = GuestAgentClient::new(host_io).expect("client");
        let mut guest =
            FrameStream::with_default_limit(guest_io, ProtocolPeer::Guest).expect("guest stream");

        let client_task = spawn(async move { client.ping().await });
        let request = guest.recv().await.expect("request");
        assert_eq!(request.request_id(), 1);
        assert_eq!(request.into_message(), AgentMessage::Ping);
        guest
            .send(guest_frame(1, AgentMessage::Pong))
            .await
            .expect("send pong");

        client_task
            .await
            .expect("client task")
            .expect("ping succeeded");
    }

    #[tokio::test]
    async fn start_process_multiplexes_events_with_later_requests() {
        let (host_io, guest_io) = duplex(4096);
        let client = GuestAgentClient::new(host_io).expect("client");
        let mut guest =
            FrameStream::with_default_limit(guest_io, ProtocolPeer::Guest).expect("guest stream");

        let client_task = spawn({
            let client = client;
            async move {
                let process = client
                    .start_process(
                        "sleep 1".to_owned(),
                        None,
                        BTreeMap::new(),
                        Some(1_000),
                        1024,
                    )
                    .await
                    .expect("start process");
                assert!(!process.execution_id.is_empty());
                assert_ne!(process.pid, 0);

                client.ping().await.expect("concurrent ping");
                let mut events = process.events;
                let stdout = events.recv().await.expect("stdout event");
                assert!(matches!(stdout, AgentMessage::ProcessStdout { .. }));
                let exit = events.recv().await.expect("exit event");
                assert!(matches!(exit, AgentMessage::ProcessExit { .. }));
            }
        });

        let start_request = guest.recv().await.expect("start request");
        let start_id = start_request.request_id();
        assert!(matches!(
            start_request.into_message(),
            AgentMessage::StartProcess { .. }
        ));
        guest
            .send(guest_frame(
                start_id,
                AgentMessage::ProcessStarted {
                    execution_id: "execution-1".to_owned(),
                    pid: 42,
                },
            ))
            .await
            .expect("send started");

        let ping_request = guest.recv().await.expect("ping request");
        let ping_id = ping_request.request_id();
        assert_eq!(ping_request.into_message(), AgentMessage::Ping);
        guest
            .send(guest_frame(ping_id, AgentMessage::Pong))
            .await
            .expect("send pong");

        guest
            .send(guest_frame(
                start_id,
                AgentMessage::ProcessStdout {
                    execution_id: "execution-1".to_owned(),
                    chunk: "stdout".to_owned(),
                },
            ))
            .await
            .expect("send stdout");
        guest
            .send(guest_frame(
                start_id,
                AgentMessage::ProcessExit {
                    execution_id: "execution-1".to_owned(),
                    result: ProcessResult::default(),
                },
            ))
            .await
            .expect("send exit");

        client_task.await.expect("client task");
    }

    #[tokio::test]
    async fn cancel_process_can_be_sent_after_start() {
        let (host_io, guest_io) = duplex(4096);
        let client = GuestAgentClient::new(host_io).expect("client");
        let mut guest =
            FrameStream::with_default_limit(guest_io, ProtocolPeer::Guest).expect("guest stream");

        let client_task = spawn({
            let client = client;
            async move {
                let process = client
                    .start_process(
                        "sleep 10".to_owned(),
                        None,
                        BTreeMap::new(),
                        Some(10_000),
                        1024,
                    )
                    .await
                    .expect("start process");
                assert!(
                    client
                        .cancel_process(&process.execution_id, true)
                        .await
                        .expect("cancel process")
                );
            }
        });

        let start_request = guest.recv().await.expect("start request");
        let start_id = start_request.request_id();
        guest
            .send(guest_frame(
                start_id,
                AgentMessage::ProcessStarted {
                    execution_id: "execution-1".to_owned(),
                    pid: 42,
                },
            ))
            .await
            .expect("send started");

        let cancel_request = guest.recv().await.expect("cancel request");
        let cancel_id = cancel_request.request_id();
        assert!(matches!(
            cancel_request.into_message(),
            AgentMessage::CancelProcess { .. }
        ));
        guest
            .send(guest_frame(
                cancel_id,
                AgentMessage::CancelProcessResult { cancelled: true },
            ))
            .await
            .expect("send cancelled");

        client_task.await.expect("client task");
    }

    #[tokio::test]
    async fn connection_close_fails_pending_request() {
        let (host_io, guest_io) = duplex(4096);
        let client = GuestAgentClient::new(host_io).expect("client");
        drop(guest_io);

        let error = client.ping().await.expect_err("connection closed");
        assert!(matches!(error, GuestAgentClientError::ConnectionClosed));
    }

    #[tokio::test]
    async fn request_rejects_start_process() {
        let (host_io, guest_io) = duplex(4096);
        let client = GuestAgentClient::new(host_io).expect("client");
        drop(guest_io);

        let error = client
            .request(AgentMessage::StartProcess {
                command: String::new(),
                cwd: None,
                env: BTreeMap::new(),
                timeout_ms: None,
                max_output_bytes: 0,
            })
            .await
            .expect_err("start process rejected");

        assert!(matches!(error, GuestAgentClientError::Protocol(_)));
    }
}
