# VM Guest Agent Binary

## Status

Implemented as the Linux-only `llm-harness-vm-agent` binary in the
`llm-harness-vm-agent-service` crate. It binds an AF_VSOCK listener and serves
the guest-agent protocol for one workspace root at a time.

## CLI

| Option | Default | Meaning |
|---|---:|---|
| `--root <path>` | `/workspace` | Workspace root opened without following a final symlink |
| `--cid <cid>` | `u32::MAX` | AF_VSOCK CID; `u32::MAX` means `VMADDR_CID_ANY` |
| `--port <port>` | `6000` | Nonzero AF_VSOCK listener port |

## Connection policy

The binary accepts one host connection at a time. This avoids cross-connection
process-state leakage while preserving the service's existing clean-close and
cleanup semantics. A clean close disconnects processes started by that
connection; a transport error is logged and the listener continues.

The handler is shared across sequential connections, so workspace files and
process state persist only within the lifetime of the guest agent process. A
host should still issue `Cleanup` before closing if it needs deterministic
temporary-directory removal.

## Non-goals

- No host-side Firecracker lifecycle manager is added.
- No authentication is added at this layer; isolation comes from the per-VM
  AF_VSOCK channel and the Firecracker lifecycle/config contract.
- No real host-to-guest integration test is claimed without a Firecracker VM.

## Verification

- Linux binary unit tests cover default and custom CLI parsing.
- Existing service, filesystem, process, and transport tests cover protocol
  dispatch, cleanup, and AF_VSOCK framing behavior.
