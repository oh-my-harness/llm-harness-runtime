# VM Guest Agent AF_VSOCK Transport

## Status

Implemented as the Linux-only `llm_harness_vm_agent_transport::vsock` module.
It provides an AF_VSOCK stream and listener that satisfy the same
`AsyncRead` / `AsyncWrite` contract used by `FrameStream`. It is not a guest
agent, Firecracker lifecycle manager, or VM backend.

## Address policy

`VsockAddress` requires a nonzero port. `VMADDR_CID_ANY` is allowed for a
guest-side listener, but `VsockStream::connect` rejects it because it does not
identify a concrete guest. Accepted connections must report an AF_VSOCK peer
address; otherwise the adapter fails closed instead of wrapping an unrelated
socket family.

## Connection and I/O semantics

`VsockStream::connect` creates a Linux `AF_VSOCK` / `SOCK_STREAM` socket,
performs a nonblocking connect, waits for writability through Tokio's
`AsyncFd`, and checks `SO_ERROR` before returning the stream. `VsockListener`
binds and listens nonblockingly and accepts through the same readiness loop.

The stream implementations:

- retry `EINTR` internally;

- retry readiness when the kernel reports a spurious `EWOULDBLOCK`;

- preserve `AsyncRead` / `AsyncWrite` semantics for zero-length buffers;

- shutdown only the write half for `AsyncWrite::poll_shutdown`.

These details keep `FrameStream` independent of socket-family behavior while
still providing production-grade async I/O over the real Linux socket API.

## Testing boundary

Linux unit tests cover address mapping and validation, CID policy, FrameStream
round trips, and clean close over a real Unix socket pair. A Unix listener test
exercises the same `AsyncFd` connect, accept, read, and write paths used by
AF_VSOCK.

This host can create AF_VSOCK sockets, but it has no vsock loopback transport
or Firecracker guest. Therefore the adapter is not yet validated against a
real host-to-guest connection. That validation belongs to the Firecracker
integration test slice and must not be claimed by the Unix-socket tests.

Callers remain responsible for connection deadlines and force-stop decisions.
No authentication is added at this layer; isolation comes from the per-VM
vsock channel and the Firecracker lifecycle/config contract.
