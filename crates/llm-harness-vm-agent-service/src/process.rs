//! Linux process execution for the VM guest agent.
//!
//! The manager executes commands through `/bin/sh`, confines the working
//! directory to the workspace root, clears inherited environment variables,
//! and streams bounded UTF-8 output while the process is running.

use std::collections::{BTreeMap, HashMap};
use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use encoding_rs::UTF_8;
use llm_harness_vm_agent_protocol::{AgentMessage, MAX_PROCESS_OUTPUT_BYTES, ProcessResult};
use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{self, FileType, Mode, OFlags, ResolveFlags};
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::{GuestAgentHandler, HandlerError, HandlerOutcome, ProcessDisconnect};

const WORKSPACE_RESOLVE_FLAGS: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_XDEV);
const MINIMAL_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const GRACEFUL_CANCEL_TIMEOUT: Duration = Duration::from_secs(5);
const EVENT_CHANNEL_CAPACITY: usize = 128;
const COMMAND_CHANNEL_CAPACITY: usize = 8;
const OUTPUT_CHUNK_BYTES: usize = 64 * 1024;
const MAX_WORKSPACE_PATH_BYTES: usize = 4096;
const MAX_WORKSPACE_PATH_COMPONENTS: usize = 256;

/// A Linux process manager rooted at a workspace directory.
pub struct ProcessManager {
    root_fd: Arc<OwnedFd>,
    processes: Arc<Mutex<HashMap<String, ProcessHandle>>>,
}

#[derive(Clone)]
struct ProcessHandle {
    live: Arc<AtomicBool>,
    commands: mpsc::Sender<ProcessCommand>,
}

#[derive(Clone, Copy)]
enum ProcessCommand {
    Cancel { force: bool },
}

#[derive(Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

impl ProcessManager {
    /// Open a workspace root without following a final symlink.
    pub fn new(root: impl AsRef<std::path::Path>) -> Result<Self, HandlerError> {
        let root_fd = fs::open(
            root.as_ref(),
            OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| filesystem_error("open process workspace root", error.into()))?;
        Ok(Self {
            root_fd: Arc::new(root_fd),
            processes: Arc::default(),
        })
    }

    fn start_process(
        &self,
        command: String,
        cwd: Option<String>,
        env: BTreeMap<String, String>,
        timeout_ms: Option<u64>,
        max_output_bytes: u64,
    ) -> Result<HandlerOutcome, HandlerError> {
        if max_output_bytes > MAX_PROCESS_OUTPUT_BYTES {
            return Err(HandlerError::operation(
                "process_error",
                "process output limit exceeds the protocol maximum",
            ));
        }
        for key in env.keys() {
            if key.is_empty() || key.contains('=') {
                return Err(HandlerError::operation(
                    "process_error",
                    "environment variable name is empty or contains '='",
                ));
            }
        }

        let cwd_fd = self.open_cwd(cwd.as_deref())?;
        let cwd_path =
            std::path::PathBuf::from(format!("/proc/self/fd/{}", cwd_fd.as_fd().as_raw_fd()));
        let mut process_command = Command::new("/bin/sh");
        process_command
            .arg("-c")
            .arg(&command)
            .current_dir(&cwd_path)
            .env_clear()
            .env("PATH", MINIMAL_PATH)
            .envs(&env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);

        let mut child = process_command
            .spawn()
            .map_err(|error| filesystem_error("spawn process", error))?;
        let Some(pid) = child.id() else {
            let _ = child.start_kill();
            return Err(HandlerError::operation(
                "process_error",
                "spawned process exited before its identifier was available",
            ));
        };
        let child = ManagedChild::new(child, pid);

        let execution_id = Uuid::now_v7().to_string();
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        let (disconnect_tx, disconnect_rx) = oneshot::channel();
        let live = Arc::new(AtomicBool::new(true));
        let handle = ProcessHandle {
            live: Arc::clone(&live),
            commands: command_tx,
        };
        self.store_process(&execution_id, handle)?;

        let process_state = ProcessState {
            execution_id: execution_id.clone(),
            pid,
            processes: Arc::clone(&self.processes),
            live: Arc::clone(&live),
        };
        tokio::spawn(run_process(
            child,
            process_state,
            timeout_ms,
            max_output_bytes,
            command_rx,
            disconnect_rx,
            event_tx,
        ));

        Ok(HandlerOutcome::ProcessStarted {
            response: AgentMessage::ProcessStarted { execution_id, pid },
            events: event_rx,
            disconnect: ProcessDisconnect {
                sender: disconnect_tx,
            },
        })
    }

    async fn cancel_process(
        &self,
        execution_id: &str,
        force: bool,
    ) -> Result<HandlerOutcome, HandlerError> {
        let handle = self
            .processes
            .lock()
            .map_err(lock_poisoned)?
            .get(execution_id)
            .cloned();
        let cancelled = match handle {
            Some(handle) if handle.live.swap(false, Ordering::SeqCst) => handle
                .commands
                .send(ProcessCommand::Cancel { force })
                .await
                .is_ok(),
            _ => false,
        };
        Ok(HandlerOutcome::Message(AgentMessage::CancelProcessResult {
            cancelled,
        }))
    }

    async fn cleanup_processes(&self) -> Result<HandlerOutcome, HandlerError> {
        let handles: Vec<ProcessHandle> = self
            .processes
            .lock()
            .map_err(lock_poisoned)?
            .values()
            .cloned()
            .collect();
        for handle in handles {
            if handle.live.swap(false, Ordering::SeqCst) {
                let command = ProcessCommand::Cancel { force: true };
                let _ = handle.commands.send(command).await;
            }
        }
        Ok(HandlerOutcome::Message(AgentMessage::CleanupResult))
    }

    fn store_process(&self, execution_id: &str, handle: ProcessHandle) -> Result<(), HandlerError> {
        self.processes
            .lock()
            .map_err(lock_poisoned)?
            .insert(execution_id.to_owned(), handle);
        Ok(())
    }

    fn open_cwd(&self, cwd: Option<&str>) -> Result<OwnedFd, HandlerError> {
        let components = workspace_components(cwd.unwrap_or("/workspace"))?;
        let mut current = self
            .root_fd
            .try_clone()
            .map_err(|error| filesystem_error("clone workspace root", error))?;
        for component in components {
            current = open_workspace_dir(&current, &component)?;
        }
        Ok(current)
    }
}

#[async_trait]
impl GuestAgentHandler for ProcessManager {
    async fn handle(&self, request: AgentMessage) -> Result<HandlerOutcome, HandlerError> {
        request
            .validate()
            .map_err(|error| HandlerError::operation("protocol_error", error.to_string()))?;

        match request {
            AgentMessage::StartProcess {
                command,
                cwd,
                env,
                timeout_ms,
                max_output_bytes,
            } => self.start_process(command, cwd, env, timeout_ms, max_output_bytes),
            AgentMessage::CancelProcess {
                execution_id,
                force,
            } => self.cancel_process(&execution_id, force).await,
            AgentMessage::Status => {
                let active_processes = self.processes.lock().map_err(lock_poisoned)?.len() as u32;
                Ok(HandlerOutcome::Message(AgentMessage::StatusResult {
                    agent_version: env!("CARGO_PKG_VERSION").to_owned(),
                    active_processes,
                }))
            }
            AgentMessage::Ping => Ok(HandlerOutcome::Message(AgentMessage::Pong)),
            AgentMessage::Cleanup => self.cleanup_processes().await,
            _ => Err(HandlerError::unsupported("operation")),
        }
    }
}

struct ProcessState {
    execution_id: String,
    pid: u32,
    processes: Arc<Mutex<HashMap<String, ProcessHandle>>>,
    live: Arc<AtomicBool>,
}

struct ManagedChild {
    child: Child,
    pid: u32,
    cleanup_on_drop: bool,
}

impl ManagedChild {
    fn new(child: Child, pid: u32) -> Self {
        Self {
            child,
            pid,
            cleanup_on_drop: true,
        }
    }

    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    fn start_kill(&mut self) -> std::io::Result<()> {
        self.child.start_kill()
    }

    fn mark_reaped(&mut self) {
        self.cleanup_on_drop = false;
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            kill_process_group(self.pid, libc::SIGKILL);
            let _ = self.start_kill();
        }
    }
}

impl Drop for ProcessState {
    fn drop(&mut self) {
        self.live.store(false, Ordering::SeqCst);
        if let Ok(mut processes) = self.processes.lock()
            && let Some(handle) = processes.get(&self.execution_id)
            && Arc::ptr_eq(&handle.live, &self.live)
        {
            processes.remove(&self.execution_id);
        }
    }
}

async fn run_process(
    mut child: ManagedChild,
    state: ProcessState,
    timeout_ms: Option<u64>,
    max_output_bytes: u64,
    mut command_rx: mpsc::Receiver<ProcessCommand>,
    mut disconnect_rx: oneshot::Receiver<()>,
    event_tx: mpsc::Sender<AgentMessage>,
) {
    let execution_id = state.execution_id.clone();
    let pid = state.pid;
    let stdout = child.child.stdout.take();
    let stderr = child.child.stderr.take();
    let stdout_task = stdout.map(|stream| {
        spawn_output_reader(
            stream,
            execution_id.clone(),
            max_output_bytes,
            event_tx.clone(),
            StreamKind::Stdout,
        )
    });
    let stderr_task = stderr.map(|stream| {
        spawn_output_reader(
            stream,
            execution_id.clone(),
            max_output_bytes,
            event_tx.clone(),
            StreamKind::Stderr,
        )
    });

    let timeout =
        timeout_ms.map(|milliseconds| Duration::from_nanos(milliseconds.saturating_mul(1_000_000)));
    let completion = tokio::select! {
        status = child.wait() => Completion::Exited(status),
        command = command_rx.recv() => match command {
            Some(ProcessCommand::Cancel { force }) => Completion::Cancelled(force),
            None => Completion::Exited(child.wait().await),
        },
        _ = &mut disconnect_rx => Completion::Disconnected,
        _ = wait_for_timeout(timeout) => Completion::TimedOut,
    };

    state.live.store(false, Ordering::SeqCst);

    let mut terminal_event = None;
    let result = match completion {
        Completion::Exited(status) => match status {
            Ok(status) => {
                kill_process_group(pid, libc::SIGKILL);
                child.mark_reaped();
                Some(ProcessResult {
                    exit_code: exit_code(&status),
                    stdout_truncated_bytes: 0,
                    stderr_truncated_bytes: 0,
                })
            }
            Err(error) => {
                kill_process_group(pid, libc::SIGKILL);
                let _ = child.start_kill();
                let _ = child.wait().await;
                child.mark_reaped();
                terminal_event = Some(process_error(&execution_id, "wait process", &error));
                None
            }
        },
        Completion::Cancelled(force) => {
            let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
            kill_process_group(pid, signal);
            let status = if force {
                child.wait().await
            } else {
                match tokio::time::timeout(GRACEFUL_CANCEL_TIMEOUT, child.wait()).await {
                    Ok(status) => status,
                    Err(_) => {
                        kill_process_group(pid, libc::SIGKILL);
                        child.wait().await
                    }
                }
            };
            kill_process_group(pid, libc::SIGKILL);
            child.mark_reaped();
            let status = match status {
                Ok(status) => Some(status),
                Err(_) => {
                    let _ = child.start_kill();
                    child.wait().await.ok()
                }
            };
            Some(ProcessResult {
                exit_code: status.as_ref().map(exit_code).unwrap_or(-1),
                stdout_truncated_bytes: 0,
                stderr_truncated_bytes: 0,
            })
        }
        Completion::TimedOut => {
            kill_process_group(pid, libc::SIGKILL);
            let status = child.wait().await.ok();
            kill_process_group(pid, libc::SIGKILL);
            child.mark_reaped();
            let result = ProcessResult {
                exit_code: status.as_ref().map(exit_code).unwrap_or(-1),
                stdout_truncated_bytes: 0,
                stderr_truncated_bytes: 0,
            };
            terminal_event = Some(AgentMessage::ProcessTimeout {
                execution_id: execution_id.clone(),
                result,
            });
            None
        }
        Completion::Disconnected => {
            kill_process_group(pid, libc::SIGKILL);
            let _ = child.wait().await;
            child.mark_reaped();
            None
        }
    };

    let stdout_truncated = join_output_reader(stdout_task).await;
    let stderr_truncated = join_output_reader(stderr_task).await;
    let output_error = if let Some(Err(error)) = stdout_truncated.as_ref() {
        Some(error.to_string())
    } else if let Some(Err(error)) = stderr_truncated.as_ref() {
        Some(error.to_string())
    } else {
        None
    };
    if let Some(mut result) = result {
        result.stdout_truncated_bytes = stdout_truncated
            .and_then(|result| result.ok())
            .unwrap_or_default();
        result.stderr_truncated_bytes = stderr_truncated
            .and_then(|result| result.ok())
            .unwrap_or_default();
        terminal_event = Some(AgentMessage::ProcessExit {
            execution_id,
            result,
        });
    } else if terminal_event.is_none()
        && let Some(output_error) = output_error
    {
        terminal_event = Some(process_error(
            &state.execution_id,
            "read process output",
            output_error,
        ));
    }
    if let Some(terminal_event) = terminal_event {
        send_event(&event_tx, terminal_event).await;
    }
}

#[derive(Debug)]
enum Completion {
    Exited(std::io::Result<std::process::ExitStatus>),
    Cancelled(bool),
    TimedOut,
    Disconnected,
}

async fn wait_for_timeout(timeout: Option<Duration>) {
    match timeout {
        Some(timeout) => tokio::time::sleep(timeout).await,
        None => std::future::pending().await,
    }
}

fn spawn_output_reader<R>(
    stream: R,
    execution_id: String,
    max_output_bytes: u64,
    event_tx: mpsc::Sender<AgentMessage>,
    kind: StreamKind,
) -> JoinHandle<std::io::Result<u64>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(read_output(
        stream,
        execution_id,
        max_output_bytes,
        event_tx,
        kind,
    ))
}

async fn read_output<R>(
    mut stream: R,
    execution_id: String,
    max_output_bytes: u64,
    event_tx: mpsc::Sender<AgentMessage>,
    kind: StreamKind,
) -> std::io::Result<u64>
where
    R: AsyncRead + Unpin,
{
    let mut decoder = UTF_8.new_decoder_without_bom_handling();
    let mut forwarded_bytes = 0_u64;
    let mut truncated_bytes = 0_u64;
    let mut pending_raw_bytes = 0_u64;
    let mut forwarding = true;
    let mut buffer = vec![0_u8; OUTPUT_CHUNK_BYTES];

    loop {
        let read_len = stream.read(&mut buffer).await?;
        if read_len == 0 {
            break;
        }
        let mut text = String::with_capacity(read_len.saturating_mul(4) + 4);
        let (_, consumed_bytes, _) =
            decoder.decode_to_string(&buffer[..read_len], &mut text, false);
        let accounted_raw_bytes = pending_raw_bytes + consumed_bytes as u64;
        pending_raw_bytes = accounted_raw_bytes;

        let remaining = max_output_bytes.saturating_sub(forwarded_bytes);
        if text.is_empty() {
            continue;
        }
        if text.len() as u64 <= remaining {
            if forwarding && send_output(&event_tx, &execution_id, &text, kind).await {
                forwarded_bytes += accounted_raw_bytes;
            } else {
                truncated_bytes += accounted_raw_bytes;
            }
            pending_raw_bytes = 0;
        } else {
            let mut cut = remaining.min(usize::MAX as u64) as usize;
            while cut > 0 && !text.is_char_boundary(cut) {
                cut -= 1;
            }
            let chunk = &text[..cut];
            let chunk_bytes = chunk.len() as u64;
            if forwarding && !chunk.is_empty() {
                forwarding = send_output(&event_tx, &execution_id, chunk, kind).await;
            }
            forwarded_bytes += chunk_bytes;
            truncated_bytes += accounted_raw_bytes.saturating_sub(chunk_bytes);
            pending_raw_bytes = 0;
        }
    }

    let mut text = String::new();
    let _ = decoder.decode_to_string(&[], &mut text, true);
    let remaining = max_output_bytes.saturating_sub(forwarded_bytes);
    if text.is_empty() {
        return Ok(truncated_bytes);
    }
    if text.len() as u64 <= remaining && forwarding {
        if !send_output(&event_tx, &execution_id, &text, kind).await {
            truncated_bytes += pending_raw_bytes;
        }
    } else {
        let mut cut = remaining.min(usize::MAX as u64) as usize;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        let chunk_bytes = cut as u64;
        if forwarding && cut > 0 {
            send_output(&event_tx, &execution_id, &text[..cut], kind).await;
        }
        truncated_bytes += pending_raw_bytes.saturating_sub(chunk_bytes);
    }
    Ok(truncated_bytes)
}

async fn send_output(
    event_tx: &mpsc::Sender<AgentMessage>,
    execution_id: &str,
    chunk: &str,
    kind: StreamKind,
) -> bool {
    let message = match kind {
        StreamKind::Stdout => AgentMessage::ProcessStdout {
            execution_id: execution_id.to_owned(),
            chunk: chunk.to_owned(),
        },
        StreamKind::Stderr => AgentMessage::ProcessStderr {
            execution_id: execution_id.to_owned(),
            chunk: chunk.to_owned(),
        },
    };
    event_tx.send(message).await.is_ok()
}

async fn send_event(event_tx: &mpsc::Sender<AgentMessage>, message: AgentMessage) -> bool {
    event_tx.send(message).await.is_ok()
}

fn process_error(
    execution_id: &str,
    operation: &str,
    error: impl std::fmt::Display,
) -> AgentMessage {
    AgentMessage::ProcessError {
        execution_id: execution_id.to_owned(),
        code: "process_error".to_owned(),
        message: format!("{operation}: {error}"),
    }
}

async fn join_output_reader(
    task: Option<JoinHandle<std::io::Result<u64>>>,
) -> Option<std::io::Result<u64>> {
    match task {
        Some(task) => match task.await {
            Ok(result) => Some(result),
            Err(error) => Some(Err(std::io::Error::other(error))),
        },
        None => None,
    }
}

fn exit_code(status: &std::process::ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| -status.signal().unwrap_or(-1))
}

fn kill_process_group(pid: u32, signal: i32) {
    let process_group_id = -(pid as libc::pid_t);
    let _ = unsafe { libc::kill(process_group_id, signal) };
}

fn workspace_components(path: &str) -> Result<Vec<CString>, HandlerError> {
    if path.len() > MAX_WORKSPACE_PATH_BYTES {
        return Err(invalid_path("workspace path is too long"));
    }
    if path == "/workspace" {
        return Ok(Vec::new());
    }
    let relative = path
        .strip_prefix("/workspace/")
        .ok_or_else(|| invalid_path("path must be below /workspace"))?;
    if relative.is_empty() {
        return Err(invalid_path("workspace path component is empty"));
    }

    let mut components = Vec::new();
    for component in relative.split('/') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.contains('\\')
            || component.contains('\0')
        {
            return Err(invalid_path(
                "workspace path contains a forbidden component",
            ));
        }
        components.push(
            CString::new(component.as_bytes())
                .map_err(|_| invalid_path("workspace path contains a NUL byte"))?,
        );
    }
    if components.len() > MAX_WORKSPACE_PATH_COMPONENTS {
        return Err(invalid_path("workspace path has too many components"));
    }
    Ok(components)
}

fn open_workspace_dir(parent: &OwnedFd, name: &CString) -> Result<OwnedFd, HandlerError> {
    let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let directory_fd = open_workspace_at(parent, name, flags, Mode::empty())
        .map_err(|error| filesystem_error("open process working directory", error))?;
    let stat = fs::fstat(&directory_fd).map_err(|error| {
        filesystem_error("read process working directory metadata", error.into())
    })?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
        return Err(HandlerError::operation(
            "process_error",
            "process working directory is not a directory",
        ));
    }
    Ok(directory_fd)
}

fn open_workspace_at<Fd: AsFd>(
    parent: &Fd,
    name: &std::ffi::CStr,
    flags: OFlags,
    mode: Mode,
) -> std::io::Result<OwnedFd> {
    match fs::openat2(parent, name, flags, mode, WORKSPACE_RESOLVE_FLAGS) {
        Ok(file_fd) => Ok(file_fd),
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
            fs::openat(parent, name, flags, mode).map_err(std::io::Error::from)
        }
        Err(error) => Err(std::io::Error::from(error)),
    }
}

fn lock_poisoned<T>(error: std::sync::PoisonError<T>) -> HandlerError {
    HandlerError::operation(
        "handler_failure",
        format!("process state lock poisoned: {error}"),
    )
}

fn invalid_path(message: &'static str) -> HandlerError {
    HandlerError::operation("invalid_path", message)
}

fn filesystem_error(operation: &'static str, error: std::io::Error) -> HandlerError {
    HandlerError::operation("process_error", format!("{operation}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_vm_agent_protocol::{Frame, ProtocolPeer};
    use llm_harness_vm_agent_transport::FrameStream;
    use tokio::io::{AsyncWriteExt, duplex};
    use tokio::spawn;

    fn manager(root: &std::path::Path) -> ProcessManager {
        ProcessManager::new(root).expect("process manager")
    }

    fn start_request(
        command: &str,
        cwd: Option<&str>,
        timeout_ms: Option<u64>,
        max_output_bytes: u64,
    ) -> AgentMessage {
        AgentMessage::StartProcess {
            command: command.to_owned(),
            cwd: cwd.map(str::to_owned),
            env: Default::default(),
            timeout_ms,
            max_output_bytes,
        }
    }

    fn started(
        outcome: HandlerOutcome,
    ) -> (String, mpsc::Receiver<AgentMessage>, ProcessDisconnect) {
        match outcome {
            HandlerOutcome::ProcessStarted {
                response: AgentMessage::ProcessStarted { execution_id, .. },
                events,
                disconnect,
            } => (execution_id, events, disconnect),
            HandlerOutcome::Message(message) => {
                panic!("expected process start, found {message:?}")
            }
            _ => panic!("expected process start"),
        }
    }

    fn message(outcome: HandlerOutcome) -> AgentMessage {
        match outcome {
            HandlerOutcome::Message(message) => message,
            _ => panic!("expected a single response"),
        }
    }

    async fn next_message(events: &mut mpsc::Receiver<AgentMessage>) -> AgentMessage {
        events.recv().await.expect("process event")
    }

    #[tokio::test]
    async fn normal_exit_streams_output_and_isolates_environment() {
        let root = tempfile::tempdir().expect("workspace root");
        std::fs::create_dir(root.path().join("cwd")).expect("create cwd");
        std::fs::write(root.path().join("cwd/marker"), b"marker").expect("write marker");
        let manager = manager(root.path());
        let mut env = BTreeMap::new();
        env.insert("SECRET".to_owned(), "value".to_owned());

        let outcome = manager
            .handle(AgentMessage::StartProcess {
                command: r#"printf 'stdout'; printf 'stderr' >&2; test "$(cat marker)" = marker; test -z "$HOME"; test "$SECRET" = value; test "$PATH" = '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin'"#.to_owned(),
                cwd: Some("/workspace/cwd".to_owned()),
                env,
                timeout_ms: Some(10_000),
                max_output_bytes: 1024,
            })
            .await
            .expect("start process");
        let (execution_id, mut events, _disconnect) = started(outcome);
        let mut stdout = String::new();
        let mut stderr = String::new();
        let result = loop {
            match next_message(&mut events).await {
                AgentMessage::ProcessStdout { chunk, .. } => stdout.push_str(&chunk),
                AgentMessage::ProcessStderr { chunk, .. } => stderr.push_str(&chunk),
                AgentMessage::ProcessExit { result, .. } => break result,
                message => panic!("unexpected process event: {message:?}"),
            }
        };

        assert_eq!(stdout, "stdout");
        assert_eq!(stderr, "stderr");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout_truncated_bytes, 0);
        assert_eq!(result.stderr_truncated_bytes, 0);
        assert!(!execution_id.is_empty());
    }

    #[tokio::test]
    async fn process_timeout_kills_the_process_group() {
        let root = tempfile::tempdir().expect("workspace root");
        let manager = manager(root.path());
        let outcome = manager
            .handle(start_request("sleep 10", None, Some(80), 1024))
            .await
            .expect("start process");
        let (_execution_id, mut events, _disconnect) = started(outcome);

        match next_message(&mut events).await {
            AgentMessage::ProcessTimeout { result, .. } => {
                assert_eq!(result.exit_code, -9);
                assert_eq!(result.stdout_truncated_bytes, 0);
                assert_eq!(result.stderr_truncated_bytes, 0);
            }
            message => panic!("unexpected process event: {message:?}"),
        }
        assert!(events.recv().await.is_none(), "timeout is terminal");
    }

    #[tokio::test]
    async fn timeout_event_is_sent_after_streamed_output() {
        let root = tempfile::tempdir().expect("workspace root");
        let manager = manager(root.path());
        let outcome = manager
            .handle(start_request("printf out; sleep 10", None, Some(100), 1024))
            .await
            .expect("start process");
        let (_execution_id, mut events, _disconnect) = started(outcome);

        match next_message(&mut events).await {
            AgentMessage::ProcessStdout { chunk, .. } => assert_eq!(chunk, "out"),
            message => panic!("unexpected process event: {message:?}"),
        }
        match next_message(&mut events).await {
            AgentMessage::ProcessTimeout { result, .. } => assert_eq!(result.exit_code, -9),
            message => panic!("unexpected process event: {message:?}"),
        }
    }

    #[tokio::test]
    async fn shell_exit_cleans_up_background_process_group_members() {
        let root = tempfile::tempdir().expect("workspace root");
        let manager = manager(root.path());
        let outcome = manager
            .handle(start_request("sleep 10 & exit 0", None, Some(2_000), 1024))
            .await
            .expect("start process");
        let (_execution_id, mut events, _disconnect) = started(outcome);

        let exit = tokio::time::timeout(Duration::from_secs(1), next_message(&mut events))
            .await
            .expect("background process does not block exit");
        match exit {
            AgentMessage::ProcessExit { result, .. } => assert_eq!(result.exit_code, 0),
            message => panic!("unexpected process event: {message:?}"),
        }
    }

    #[tokio::test]
    async fn graceful_cancellation_sends_sigterm() {
        let root = tempfile::tempdir().expect("workspace root");
        let manager = manager(root.path());
        let outcome = manager
            .handle(start_request(
                "trap 'exit 0' TERM; while :; do sleep 0.1; done",
                None,
                Some(10_000),
                1024,
            ))
            .await
            .expect("start process");
        let (execution_id, mut events, _disconnect) = started(outcome);
        tokio::time::sleep(Duration::from_millis(100)).await;

        let outcome = manager
            .handle(AgentMessage::CancelProcess {
                execution_id,
                force: false,
            })
            .await
            .expect("cancel process");
        assert_eq!(
            message(outcome),
            AgentMessage::CancelProcessResult { cancelled: true }
        );

        match next_message(&mut events).await {
            AgentMessage::ProcessStderr { .. } => {}
            AgentMessage::ProcessExit { result, .. } => {
                assert_eq!(result.exit_code, 0);
            }
            message => panic!("unexpected process event: {message:?}"),
        }
    }

    #[tokio::test]
    async fn force_cancellation_sends_sigkill() {
        let root = tempfile::tempdir().expect("workspace root");
        let manager = manager(root.path());
        let outcome = manager
            .handle(start_request("sleep 10", None, Some(10_000), 1024))
            .await
            .expect("start process");
        let (execution_id, mut events, _disconnect) = started(outcome);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let outcome = manager
            .handle(AgentMessage::CancelProcess {
                execution_id,
                force: true,
            })
            .await
            .expect("cancel process");
        assert_eq!(
            message(outcome),
            AgentMessage::CancelProcessResult { cancelled: true }
        );

        match next_message(&mut events).await {
            AgentMessage::ProcessExit { result, .. } => {
                assert_eq!(result.exit_code, -9);
            }
            message => panic!("unexpected process event: {message:?}"),
        }
    }

    #[tokio::test]
    async fn output_limits_count_discarded_bytes() {
        let root = tempfile::tempdir().expect("workspace root");
        let manager = manager(root.path());
        let outcome = manager
            .handle(start_request(
                "printf abcdef; printf 123456 >&2",
                None,
                Some(10_000),
                4,
            ))
            .await
            .expect("start process");
        let (_execution_id, mut events, _disconnect) = started(outcome);
        let mut stdout = String::new();
        let mut stderr = String::new();
        let result = loop {
            match next_message(&mut events).await {
                AgentMessage::ProcessStdout { chunk, .. } => stdout.push_str(&chunk),
                AgentMessage::ProcessStderr { chunk, .. } => stderr.push_str(&chunk),
                AgentMessage::ProcessExit { result, .. } => break result,
                message => panic!("unexpected process event: {message:?}"),
            }
        };

        assert_eq!(stdout, "abcd");
        assert_eq!(stderr, "1234");
        assert_eq!(result.stdout_truncated_bytes, 2);
        assert_eq!(result.stderr_truncated_bytes, 2);
    }

    #[tokio::test]
    async fn incremental_decoder_handles_split_utf8_characters() {
        let (mut writer, reader) = duplex(64);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let read = read_output(
            reader,
            "test-execution".to_owned(),
            1024,
            event_tx,
            StreamKind::Stdout,
        );
        let write = async {
            writer.write_all(&[0xe4, 0xb8]).await.expect("write first");
            tokio::time::sleep(Duration::from_millis(20)).await;
            writer.write_all(&[0xad]).await.expect("write second");
            writer.shutdown().await.expect("shutdown");
        };
        let ((), truncated) = tokio::join!(write, read);

        assert_eq!(truncated.expect("read output"), 0);
        assert_eq!(
            event_rx.recv().await,
            Some(AgentMessage::ProcessStdout {
                execution_id: "test-execution".to_owned(),
                chunk: "中".to_owned(),
            })
        );
    }

    #[tokio::test]
    async fn incremental_decoder_counts_split_utf8_at_output_limit() {
        let (mut writer, reader) = duplex(64);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let read = read_output(
            reader,
            "test-execution".to_owned(),
            1,
            event_tx,
            StreamKind::Stdout,
        );
        let write = async {
            writer.write_all(&[0xe4, 0xb8]).await.expect("write first");
            tokio::time::sleep(Duration::from_millis(20)).await;
            writer.write_all(&[0xad]).await.expect("write second");
            writer.shutdown().await.expect("shutdown");
        };
        let ((), truncated) = tokio::join!(write, read);

        assert_eq!(truncated.expect("read output"), 3);
        assert!(event_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn invalid_cwd_and_cwd_symlink_are_rejected() {
        let root = tempfile::tempdir().expect("workspace root");
        let outside = tempfile::tempdir().expect("outside root");
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape"))
            .expect("create cwd symlink");
        let manager = manager(root.path());

        for cwd in ["/workspace/missing", "/workspace/escape"] {
            let error = manager
                .handle(start_request("true", Some(cwd), Some(1_000), 1024))
                .await
                .expect_err("invalid cwd");
            assert_eq!(error.code(), "process_error");
        }
    }

    #[tokio::test]
    async fn invalid_environment_name_is_rejected() {
        let root = tempfile::tempdir().expect("workspace root");
        let manager = manager(root.path());
        let mut env = BTreeMap::new();
        env.insert("BAD=KEY".to_owned(), "value".to_owned());

        let error = manager
            .handle(AgentMessage::StartProcess {
                command: "true".to_owned(),
                cwd: None,
                env,
                timeout_ms: Some(1_000),
                max_output_bytes: 1024,
            })
            .await
            .expect_err("invalid environment name");
        assert_eq!(error.code(), "process_error");
    }

    #[tokio::test]
    async fn status_and_cleanup_track_live_processes() {
        let root = tempfile::tempdir().expect("workspace root");
        let manager = manager(root.path());
        let outcome = manager
            .handle(start_request("sleep 10", None, Some(10_000), 1024))
            .await
            .expect("start process");
        let (_execution_id, mut events, _disconnect) = started(outcome);

        let status = manager.handle(AgentMessage::Status).await.expect("status");
        assert_eq!(
            message(status),
            AgentMessage::StatusResult {
                agent_version: env!("CARGO_PKG_VERSION").to_owned(),
                active_processes: 1,
            }
        );

        let cleanup = manager
            .handle(AgentMessage::Cleanup)
            .await
            .expect("cleanup");
        assert_eq!(message(cleanup), AgentMessage::CleanupResult);
        match next_message(&mut events).await {
            AgentMessage::ProcessExit { result, .. } => assert_eq!(result.exit_code, -9),
            message => panic!("unexpected process event: {message:?}"),
        }

        let status = manager.handle(AgentMessage::Status).await.expect("status");
        assert_eq!(
            message(status),
            AgentMessage::StatusResult {
                agent_version: env!("CARGO_PKG_VERSION").to_owned(),
                active_processes: 0,
            }
        );
    }

    #[tokio::test]
    async fn process_events_are_multiplexed_over_the_service() {
        let root = tempfile::tempdir().expect("workspace root");
        let (host_io, guest_io) = duplex(4096);
        let mut host =
            FrameStream::with_default_limit(host_io, ProtocolPeer::Host).expect("host stream");
        let guest =
            FrameStream::with_default_limit(guest_io, ProtocolPeer::Guest).expect("guest stream");
        let service = crate::GuestAgentService::new(manager(root.path()));
        let service_task = spawn(async move { service.serve(guest).await });

        let request = Frame::new(
            ProtocolPeer::Host,
            41,
            start_request("printf out; sleep 0.2", None, Some(10_000), 1024),
        )
        .expect("start frame");
        host.send(request).await.expect("send start");
        let response = host.recv().await.expect("receive started");
        assert_eq!(response.request_id(), 41);
        assert!(matches!(
            response.into_message(),
            AgentMessage::ProcessStarted { .. }
        ));

        let response = host.recv().await.expect("receive stdout");
        let message = response.into_message();
        let AgentMessage::ProcessStdout {
            execution_id,
            chunk,
        } = message
        else {
            panic!("expected stdout event, found {message:?}")
        };
        assert!(!execution_id.is_empty());
        assert_eq!(chunk, "out");

        let request = Frame::new(ProtocolPeer::Host, 42, AgentMessage::Ping).expect("ping frame");
        host.send(request).await.expect("send ping");
        let response = host.recv().await.expect("receive pong");
        assert_eq!(response.request_id(), 42);
        assert_eq!(response.into_message(), AgentMessage::Pong);

        let response = host.recv().await.expect("receive exit");
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
}
