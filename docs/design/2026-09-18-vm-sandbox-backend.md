# VM-level Remote Sandbox Backend Design

## Goal

Define the first VM-level backend for the remote sandbox protocol used by
`RemoteSandbox` and `RemoteEnv`. This design addresses #207 and complements the
existing Linux Bwrap gateway: Bwrap remains the embedded namespace backend, while
a VM backend is required for high-threat multi-tenant deployments that need a
hardware virtualization boundary.

The first backend is **Firecracker**.

## Backend selection

| Backend | Isolation boundary | Fit | Risks / costs |
|---|---|---|---|
| Firecracker | KVM microVM per sandbox | Closest to Kimi AgentENV's disposable MicroVM model; fast boot; small device model; explicit per-VM resource and network controls | Requires KVM and disciplined image/rootfs management; no general container tooling |
| Kata Containers | KVM-backed pod/container | Strong isolation and familiar container workflow | Heavier per-sandbox stack; Kubernetes/shim assumptions can exceed the gateway's scope |
| gVisor | Linux syscall interception | Easier deployment on hosts without KVM | Weaker conceptual match to Kimi AgentENV; performance and compatibility depend on workload; still relies on host kernel interfaces |

Firecracker is selected as the first backend because it matches the target model:
one disposable Linux MicroVM per task, a minimal device surface, explicit vCPU and
memory limits, per-VM block devices, and an optional host-controlled network
device. Kata remains a possible later backend for container-oriented deployments;
gVisor is not selected for the first high-threat backend because it does not
provide the same hardware virtualization boundary.

## Threat model

### Trusted components

- The gateway control process and its authenticated configuration.
- The host Linux kernel, KVM, and the minimum Firecracker process surface.
- The signed VM image and guest agent build pipeline.
- The host-side policy enforcement and network/firewall configuration.

### Untrusted components

- All tenant-generated shell commands and code.
- The guest kernel and guest userland after a workload runs.
- Files and network traffic originating from a sandbox.
- Any data returned by the guest agent after guest compromise.

### Protected assets

- Host filesystem, processes, credentials, and device nodes.
- Other tenants' sandboxes and workspaces.
- Gateway bearer tokens, registry contents, and control-plane interfaces.
- Internal service networks that are not explicitly allowed for a sandbox.

### Security boundaries

1. **Hardware virtualization boundary**
   - Every sandbox receives its own Firecracker MicroVM.
   - The guest has no host bind mount, host namespace, or host device access.
   - Guest compromise must not expose the gateway host or another tenant.

2. **Per-sandbox storage**
   - Every sandbox receives an ephemeral root/workspace image.
   - `reset` recreates or replaces the VM and workspace image rather than trying
     to sanitize a compromised guest.
   - No host directory is shared writable with the guest.

3. **Control and data channel**
   - The gateway communicates with a per-VM guest agent over vsock.
   - The guest agent accepts only the protocol operations needed for files,
     process execution, cancellation, and process cleanup.
   - Guest-originated data is treated as untrusted input by the gateway.

4. **Policy enforcement outside the guest**
   - vCPU, memory, and disk capacity are configured by the gateway before VM start.
   - Network policy is enforced by the host at the VM network interface; an empty
     allowlist means no network device or an interface that can only reach a
     deny-by-default firewall.
   - Filesystem allow/deny policy is validated by the gateway before an operation
     is forwarded over vsock.
   - Timeout and output limits are enforced by the gateway, not only by guest code.

5. **Lifecycle cleanup**
   - Shutdown stops the Firecracker VM and removes its root/workspace artifacts.
   - A VM that fails to respond to cancellation is force-stopped.
   - Registry recovery treats unknown or unrecoverable VMs as deleted, and never
     silently reconnects a stale guest to a different workspace.

## Architecture

```text
RemoteSandbox / RemoteEnv
  └── RemoteSandboxClient
       └── sandbox gateway HTTP/SSE protocol
            └── Firecracker backend
                 ├── Firecracker API: VM lifecycle and resource limits
                 ├── per-sandbox rootfs/workspace block device
                 ├── host network/firewall policy
                 └── vsock guest agent
                      ├── file operations under /workspace
                      ├── process stdio and exit status
                      └── process kill and cleanup
```

The remote client contract remains unchanged. The backend is selected by gateway
configuration, not by upper-layer runtime code. This keeps `RemoteSandbox`,
`RemoteEnv`, tools, workflow, budget, audit, and cancellation semantics backend
agnostic.

## Protocol mapping

| Protocol operation | Firecracker backend behavior |
|---|---|
| Create sandbox | Allocate sandbox id, create ephemeral VM image, configure resources/network, start VM |
| Start sandbox | Confirm VM and guest agent readiness |
| Status | Track `Creating`, `Running`, and `Deleting` in gateway state |
| File read/write/list/remove | Gateway validates policy, forwards operation over vsock to guest agent |
| Shell execute | Guest agent starts process, streams stdout/stderr over vsock, reports exit |
| Abort | Guest agent kills process; gateway force-stops VM if the agent does not respond |
| Timeout | Gateway enforces deadline and performs the same abort path |
| Reset | Stop VM, recreate workspace/rootfs, start a replacement VM |
| Delete | Stop VM, remove image and network/firewall artifacts, remove registry record |

The existing HTTP/SSE protocol, error mapping, request size limits, and output
caps are retained. The VM backend only replaces the execution boundary behind
the gateway.

## Policy requirements

### Filesystem

- Guest paths remain anchored to `/workspace`.
- `fs_allowlist` and `fs_denylist` use component-aware path containment.
- Deny rules take precedence over allow rules.
- Symlink and traversal escapes are rejected before an operation is sent to the
  guest.
- The guest agent itself is restricted to `/workspace`; it never receives host
  paths or credentials.

### Resources

- `max_cpus` maps to Firecracker vCPU count.
- `max_memory_mb` maps to Firecracker guest memory.
- `max_processes` is enforced by the guest agent and treated as defense in depth.
- `max_disk_mb` maps to the workspace block device size; the host refuses to grow
  it beyond the configured cap.
- Configurations that cannot be enforced fail closed at sandbox creation.

### Network

- Empty `net_allowlist` creates no routable network device.
- Non-empty allowlists are enforced by a host-side deny-by-default firewall on
  the VM interface.
- DNS, metadata services, link-local addresses, and private ranges are denied
  unless explicitly allowed.
- The vsock control channel is not exposed to guest workloads.

## Implementation slices

1. **Backend abstraction**
   - Extract the gateway's concrete `BwrapSandbox` dependency into a small
     internal backend interface.
   - Preserve registry, ownership, validation, and HTTP/SSE behavior.

2. **Guest agent protocol**
   - Define a compact vsock protocol for file operations, process execution,
     stdio streaming, cancellation, and status.
   - Add bounded frames, request ids, and fail-closed decoding.

3. **Firecracker lifecycle**
   - Add image allocation, VM configuration, start, health/readiness, force stop,
     and artifact cleanup.
   - Validate host prerequisites (KVM, Firecracker binary, image, permissions).

4. **Policy and limits**
   - Enforce filesystem, network, vCPU, memory, disk, timeout, and output
     policies at or outside the VM boundary.

5. **Test harness**
   - Add a fake VM backend for deterministic protocol tests.
   - Add an opt-in integration test for real Firecracker deployments.
   - Add multi-tenant tests proving one guest cannot access another workspace.

6. **Deployment validation**
   - Validate bare-metal/KVM deployment, image integrity, host firewall rules,
     crash recovery, and cleanup under gateway restart.

## Current status

This document selects Firecracker and defines the required threat model. No
Firecracker backend implementation exists yet. The Bwrap gateway remains the only
implemented remote execution backend, and #193 remains open until VM-level
isolation, production policy enforcement, and deployment validation are complete.
