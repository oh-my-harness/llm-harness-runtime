# VM Guest Agent Wire Protocol

## Status

Implemented as the transport-independent `llm-harness-vm-agent-protocol` crate.
It is not yet connected to a guest agent, vsock transport, or Firecracker
lifecycle. The protocol is a prerequisite for those integration slices and
must remain free of HTTP and runtime-layer dependencies.

## Frame format

All integers are unsigned and big-endian. Frames use a 20-byte header followed
by one compact JSON payload:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 4 | Magic bytes `LHSV` |
| 4 | 1 | Protocol version `1` |
| 5 | 3 | Reserved; must be zero |
| 8 | 4 | Payload length in bytes |
| 12 | 8 | Request ID |
| 20 | variable | JSON payload |

The request ID correlates a host request with its response and process events.
It is transported outside JSON so correlation remains available even when a
compromised guest returns an error payload.

The JSON payload uses adjacent tagging: `"type"` selects the operation and
`"data"` carries its fields. Unknown JSON fields are rejected. The default and
recommended payload cap is 64 MiB, matching the gateway's current maximum
request body. A decoder learns the declared payload length before allocating
the payload buffer, so an oversized frame is rejected without allocating its
declared size.

Message direction is part of the protocol contract. The gateway may originate
control requests, while only the guest agent may originate responses and
process events. Frame construction and decoding both take an explicit peer
role, so a compromised guest cannot inject host-only requests into the gateway
decoder. Wire fields that represent counts or byte limits use fixed-width
integers (`u32` or `u64`), avoiding `usize` differences between 32-bit and
64-bit VM images.

## Messages

The protocol covers:

- Liveness and status: `ping`, `pong`, `status`, and `status_result`.
- File operations under `/workspace`: read, write, metadata, list, exists,
  create directory, remove, and temporary-directory creation.
- Process execution: start, started, stdout, stderr, exit, timeout, error, and
  cancellation.
- Agent cleanup and protocol-level errors.

File paths must equal `/workspace` or be a normalized subpath. The protocol
rejects backslashes, NUL bytes, `.`, `..`, empty components, and trailing
slashes before an operation reaches the guest. The guest agent must still
resolve symlinks and enforce that every open remains below `/workspace`; wire
validation is defense in depth, not a substitute for openat2-style containment.

Binary file content is represented as standard base64 in `FileContent::Binary`.
Process output limits must be nonzero and no greater than 64 MiB per stream.
The guest streams stdout and stderr as bounded chunks; exit and timeout frames
carry only the exit code and truncation counts. The gateway aggregates streamed
output and preserves the existing `RemoteSandbox` final-result semantics. This
avoids duplicating large outputs in a final frame and keeps frame size
independent of the process output limit.

## Failure policy

Decoding is fail-closed:

- Invalid magic, version, reserved bytes, zero payload length, or an oversized
  declared payload rejects the stream.
- Unknown message types and unknown fields reject the frame.
- Missing or semantically invalid fields reject the frame.
- Partial headers and payloads are retained across bounded transport reads.

Once a decoder returns an error, the caller must close that vsock connection
and stop the affected VM. Retrying on the same connection is not permitted
because the stream may no longer be frame-aligned.

## Integration contract

The gateway owns policy validation, request/output limits, deadlines, and
force-stop decisions. The guest agent owns process cleanup and defense-in-depth
path containment. Neither side may send host filesystem paths or credentials
through this protocol. A future vsock transport may adapt frames to any
bounded byte stream without changing this wire format.
