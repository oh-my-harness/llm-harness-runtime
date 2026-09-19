# VM Guest Agent Byte-Stream Transport

## Status

Implemented as the transport-independent
`llm-harness-vm-agent-transport` crate. It is not an AF_VSOCK adapter, guest
agent, or Firecracker backend. Those integration slices will reuse this frame
stream and can be tested independently.

## Scope

`FrameStream` adapts any `AsyncRead + AsyncWrite + Unpin` byte stream to the
guest-agent wire protocol. The implementation:

- retains partial headers and payloads across bounded reads;
- buffers coalesced frames for subsequent `recv` calls;
- rejects payloads above the configured limit before allocation;
- validates outbound frames as originating from the local peer;
- configures decoding to accept only the opposite peer;
- flushes each outbound frame and supports write-half shutdown;
- exposes the underlying stream through `get_ref`, `get_mut`, and
  `into_inner` for integration adapters.

The stream is deliberately generic rather than vsock-specific. This keeps the
framing behavior testable with Tokio's in-memory duplex streams and avoids
coupling protocol correctness to a Linux-only transport API.

## Failure and close semantics

Decoding follows the protocol's fail-close policy. Once the wire stream is
malformed, oversized, or directionally invalid, `recv` returns an error and the
connection must be discarded. A clean remote close reports all already decoded
pending frames first and then returns `TransportError::Closed`.

`send` encodes the frame with the same payload limit used by the decoder. It
rejects a frame before writing if its origin peer does not match the local
peer, preventing a host/guest role mix-up from being sent to the remote side.

## Future vsock adapter boundary

A future Linux adapter should construct an AF_VSOCK stream with `socket2` and
Tokio's async file-descriptor support, then pass that stream to
`FrameStream::new`. The adapter should own only connection setup, shutdown,
and platform error mapping. It must not duplicate frame decoding, direction
validation, or payload-limit checks.
