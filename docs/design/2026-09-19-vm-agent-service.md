# VM Agent Request-Response Service

## Status

Implemented as an independent integration slice. This is not yet a complete
guest agent and does not close the VM sandbox issue.

## Scope

`llm-harness-vm-agent-service` provides a transport-agnostic dispatcher for a
`FrameStream<ProtocolPeer::Guest>`:

1. Read one validated host request.
2. Dispatch it to an async `GuestAgentHandler`.
3. Construct a guest-originating response frame with the same request ID.
4. Send the response over the same bounded frame stream.
5. Forward process events concurrently with subsequent requests.
6. Treat clean transport close as successful termination.

The service can be shared across connections. Requests are dispatched
serially, while the transport is split into independent read and write halves.
Responses and process events share one bounded outbound queue, so wire writes
remain ordered and a host that stops reading applies backpressure instead of
causing unbounded guest memory growth.

## Failure policy

On Linux, `ProcessManager` implements `StartProcess`, `CancelProcess`,
`Status`, `Ping`, and `Cleanup`. Commands run as `/bin/sh -c` in a process
group. The working directory is resolved with `openat2` using
`RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV`, the environment is
cleared before a minimal `PATH` and the requested variables are injected, and
stdout/stderr are streamed through an incremental UTF-8 decoder.

Each stream is independently capped by `max_output_bytes`; bytes beyond the
cap are counted and discarded. `ProcessExit` and `ProcessTimeout` are terminal
events and are emitted after both output streams have been drained, or after a
bounded one-second concurrent drain timeout. The timeout prevents a descendant
that leaves the process group while retaining a pipe from delaying terminal
event delivery.
Timeouts and connection disconnects use `SIGKILL` on the process group.
Graceful cancellation sends `SIGTERM`, waits a bounded interval, and then
sends `SIGKILL`. The manager reaps the shell and cleans up background process
group members after the shell exits.

When the host cleanly closes the connection, the service disconnects every
started process and waits for the write half to finish. This terminates process
group members; VM lifecycle shutdown remains responsible for descendants that
deliberately leave the process group.

`WorkspaceAgentHandler` composes the Linux filesystem and process handlers from
one opened workspace root. Filesystem operations are routed to
`WorkspaceFileHandler`; process, cancellation, and status operations are routed
to `ProcessManager`. `Cleanup` first force-cancels live processes and then
removes guest-owned temporary directories.

Handler failures are converted to `AgentMessage::Error` with a stable handler
code and a message capped at 4 KiB. Empty or NUL-containing handler input is
sanitized before it reaches the wire. The connection stays open because an
individual operation failure does not imply stream desynchronization.

Wire decoding and direction enforcement remain fail-closed in the protocol and
transport layers. A handler that returns a host-originating message is rejected
before sending and terminates that connection. The same policy applies to an
asynchronous process event that violates the guest direction. A transport or
protocol error, including a write-half failure, also terminates the connection
and disconnects its processes; the caller is responsible for closing or
force-stopping the affected VM as appropriate.

## Non-goals

- No filesystem operations are implemented here; handlers provide those
  semantics behind the dispatcher.
- No vsock listener or guest binary is added; callers can already compose the
  complete `WorkspaceAgentHandler` with any suitable transport.
- No vsock listener or guest binary is added; any `AsyncRead + AsyncWrite`
  transport can be wrapped with `FrameStream`.
- No Firecracker lifecycle, policy enforcement, or deployment validation is
  claimed by this slice.

## Verification

The service tests cover:

- Request/response dispatch with request-ID preservation.
- Multiplexing process events with later request/response traffic.
- Normal process exit, stdout/stderr streaming, environment isolation, and
  workspace-relative working directories.
- Guest-enforced timeout, graceful and forceful cancellation, cleanup of
  background process-group members, and terminal-event ordering.
- Per-stream output truncation, including split UTF-8 characters.
- Rejection of missing or symlinked working directories and invalid
  environment-variable names.
- Stable handler error reporting with request-ID preservation.
- Refusal to serve a stream configured for the wrong local peer.
- Rejection of a handler response with the wrong protocol direction.
- Rejection of a process event with the wrong protocol direction and cleanup of
  its process.
- Service shutdown when the write half fails, including process disconnection.
