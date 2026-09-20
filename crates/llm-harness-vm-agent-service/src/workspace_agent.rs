//! Linux guest agent handler for one workspace.
//!
//! This handler composes the workspace filesystem and process managers while
//! guaranteeing that both operate on the same opened workspace root.

use std::sync::Arc;

use async_trait::async_trait;
use llm_harness_vm_agent_protocol::AgentMessage;
use rustix::fs::{self, Mode, OFlags};

use crate::{
    GuestAgentHandler, HandlerError, HandlerOutcome, ProcessManager, WorkspaceFileHandler,
};

/// A Linux guest agent rooted at one workspace directory.
pub struct WorkspaceAgentHandler {
    files: WorkspaceFileHandler,
    processes: ProcessManager,
}

impl WorkspaceAgentHandler {
    /// Open one workspace root and build the complete guest handler from it.
    pub fn new(root: impl AsRef<std::path::Path>) -> Result<Self, HandlerError> {
        let root_fd = fs::open(
            root.as_ref(),
            OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| {
            HandlerError::operation("filesystem_error", format!("open workspace root: {error}"))
        })?;
        let root_fd = Arc::new(root_fd);
        let file_root_fd = root_fd.try_clone().map_err(|error| {
            HandlerError::operation("filesystem_error", format!("clone workspace root: {error}"))
        })?;

        Ok(Self {
            files: WorkspaceFileHandler::from_root_fd(file_root_fd),
            processes: ProcessManager::from_root_fd(root_fd),
        })
    }
}

#[async_trait]
impl GuestAgentHandler for WorkspaceAgentHandler {
    async fn handle(&self, request: AgentMessage) -> Result<HandlerOutcome, HandlerError> {
        match request {
            AgentMessage::StartProcess { .. }
            | AgentMessage::CancelProcess { .. }
            | AgentMessage::Status => self.processes.handle(request).await,
            AgentMessage::Cleanup => {
                self.processes.handle(AgentMessage::Cleanup).await?;
                self.files.handle(AgentMessage::Cleanup).await
            }
            request => self.files.handle(request).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GuestAgentService;
    use llm_harness_vm_agent_protocol::{FileContent, Frame, ProtocolPeer};
    use llm_harness_vm_agent_transport::FrameStream;
    use tokio::io::duplex;
    use tokio::spawn;

    fn host_frame(request_id: u64, message: AgentMessage) -> Frame {
        Frame::new(ProtocolPeer::Host, request_id, message).expect("host frame")
    }

    #[tokio::test]
    async fn routes_file_and_process_requests_over_one_service() {
        let root = tempfile::tempdir().expect("workspace root");
        let (host_io, guest_io) = duplex(4096);
        let mut host =
            FrameStream::with_default_limit(host_io, ProtocolPeer::Host).expect("host stream");
        let guest =
            FrameStream::with_default_limit(guest_io, ProtocolPeer::Guest).expect("guest stream");
        let service = GuestAgentService::new(
            WorkspaceAgentHandler::new(root.path()).expect("workspace agent handler"),
        );
        let service_task = spawn(async move { service.serve(guest).await });

        host.send(host_frame(
            101,
            AgentMessage::WriteFile {
                path: "/workspace/agent-file".to_owned(),
                content: FileContent::Text {
                    value: "workspace".to_owned(),
                },
                append: false,
            },
        ))
        .await
        .expect("send write");
        let response = host.recv().await.expect("write response");
        assert_eq!(response.request_id(), 101);
        assert_eq!(response.into_message(), AgentMessage::WriteFileResult);

        host.send(host_frame(
            102,
            AgentMessage::StartProcess {
                command: "printf agent-output".to_owned(),
                cwd: None,
                env: Default::default(),
                timeout_ms: Some(10_000),
                max_output_bytes: 1024,
            },
        ))
        .await
        .expect("send process");
        let response = host.recv().await.expect("process started");
        assert_eq!(response.request_id(), 102);
        assert!(matches!(
            response.into_message(),
            AgentMessage::ProcessStarted { .. }
        ));
        let response = host.recv().await.expect("process stdout");
        assert_eq!(response.request_id(), 102);
        assert!(matches!(
            response.into_message(),
            AgentMessage::ProcessStdout { .. }
        ));
        let response = host.recv().await.expect("process exit");
        assert_eq!(response.request_id(), 102);
        assert!(matches!(
            response.into_message(),
            AgentMessage::ProcessExit { .. }
        ));

        host.close().await.expect("close host");
        drop(host);
        service_task
            .await
            .expect("service completed")
            .expect("service ok");
    }

    #[tokio::test]
    async fn cleanup_stops_processes_and_removes_tracked_temp_directories() {
        let root = tempfile::tempdir().expect("workspace root");
        let handler = WorkspaceAgentHandler::new(root.path()).expect("workspace agent handler");
        std::fs::write(root.path().join("user-file"), b"keep").expect("write user file");

        let temp_path = match handler
            .handle(AgentMessage::CreateTempDir {
                prefix: "agent".to_owned(),
            })
            .await
            .expect("create temp dir")
        {
            HandlerOutcome::Message(AgentMessage::CreateTempDirResult { path }) => path,
            outcome => panic!("unexpected temp-dir outcome: {outcome:?}"),
        };
        let (execution_id, mut events, _disconnect) = match handler
            .handle(AgentMessage::StartProcess {
                command: "sleep 10".to_owned(),
                cwd: None,
                env: Default::default(),
                timeout_ms: Some(10_000),
                max_output_bytes: 1024,
            })
            .await
            .expect("start process")
        {
            HandlerOutcome::ProcessStarted {
                response: AgentMessage::ProcessStarted { execution_id, .. },
                events,
                disconnect,
            } => (execution_id, events, disconnect),
            outcome => panic!("unexpected process outcome: {outcome:?}"),
        };
        assert!(!execution_id.is_empty());

        match handler
            .handle(AgentMessage::Cleanup)
            .await
            .expect("cleanup")
        {
            HandlerOutcome::Message(AgentMessage::CleanupResult) => {}
            outcome => panic!("unexpected cleanup outcome: {outcome:?}"),
        }
        match events.recv().await.expect("process exit") {
            AgentMessage::ProcessExit { .. } => {}
            message => panic!("unexpected process event: {message:?}"),
        }

        let relative = temp_path.strip_prefix("/workspace/").expect("guest path");
        assert!(root.path().join("user-file").exists());
        assert!(!root.path().join(relative).exists());

        match handler.handle(AgentMessage::Status).await.expect("status") {
            HandlerOutcome::Message(AgentMessage::StatusResult {
                active_processes, ..
            }) => assert_eq!(active_processes, 0),
            outcome => panic!("unexpected status outcome: {outcome:?}"),
        }
    }
}
