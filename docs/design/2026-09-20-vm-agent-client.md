# VM Guest Agent Host Client

## Status

Implemented as `GuestAgentClient` in `llm-harness-vm-agent-service`. It gives
the gateway a host-side protocol client for any duplex byte stream and, on
Linux, a direct AF_VSOCK connection constructor.

## Scope

- Send host requests with monotonically increasing request IDs.
- Await correlated guest responses without blocking unrelated requests.
- Start a process and expose its asynchronous stdout/stderr/exit event stream.
- Multiplex later request/response traffic with events from a running process.
- Fail pending requests and process events when the connection closes.
- Preserve the bounded frame limits and peer-direction checks from
  `FrameStream`.

## Failure policy

Request IDs are allocated before a frame is sent, and a response with an
unknown ID is rejected as a protocol violation. Guest `Error` responses are
converted into a typed client error. Process event receivers are bounded, so a
host that stops consuming events applies backpressure instead of growing guest
memory.

When the underlying stream closes, the client resolves every pending request
with `ConnectionClosed` and emits a terminal error event to process event
receivers. A response with the wrong protocol direction is rejected by the
transport before it reaches the client.

## Non-goals

- No Firecracker lifecycle manager is added.
- No gateway HTTP/SSE integration is added.
- No host-side policy enforcement or VM resource configuration is added.

## Verification

The client tests cover:

- Ping request/response and request-ID preservation.
- Process start followed by a concurrent ping and then stdout/exit events.
- Clean connection close failing a pending request.
