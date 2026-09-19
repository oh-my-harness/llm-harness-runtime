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
5. Treat clean transport close as successful termination.

The service can be shared across connections. Each connection is dispatched
serially, which makes response ordering deterministic and prevents one slow
operation from reordering another request before its response.

## Failure policy

`StartProcess` and `CancelProcess` are rejected with the stable
`unsupported_operation` code. The response preserves the original request ID
and the connection remains usable. This is deliberate: process operations need
a process manager that emits stdout, stderr, exit, timeout, and error events
after the initial `ProcessStarted` response. Accepting only the initial
response would create a misleading partial implementation.

Handler failures are converted to `AgentMessage::Error` with a stable handler
code and a message capped at 4 KiB. Empty or NUL-containing handler input is
sanitized before it reaches the wire. The connection stays open because an
individual operation failure does not imply stream desynchronization.

Wire decoding and direction enforcement remain fail-closed in the protocol and
transport layers. A handler that returns a host-originating message is rejected
before sending and terminates that connection. A transport or protocol error
also terminates the connection; the caller is responsible for closing or
force-stopping the affected VM as appropriate.

## Non-goals

- No filesystem operations are implemented here; handlers provide those
  semantics behind the dispatcher.
- No process manager, process-event multiplexer, cancellation timeout, or
  cleanup policy is implemented.
- No vsock listener or guest binary is added; any `AsyncRead + AsyncWrite`
  transport can be wrapped with `FrameStream`.
- No Firecracker lifecycle, policy enforcement, or deployment validation is
  claimed by this slice.

## Verification

The service tests cover:

- Request/response dispatch with request-ID preservation.
- Rejection of process operations while keeping the connection usable.
- Stable handler error reporting with request-ID preservation.
- Refusal to serve a stream configured for the wrong local peer.
- Rejection of a handler response with the wrong protocol direction.
