# Mobile Agent Remote Sandbox MVP Design

## Summary

This document defines the minimum backend required to let a phone-based client
use an agent whose filesystem and shell operations run on a remote Linux host.
It intentionally keeps the first slice narrow: the phone is a thin control and
observation client, while the agent runtime and sandbox remain on a server or
user-controlled laptop. Running the full agent runtime inside the phone is a
later product decision and is not part of this MVP.

This design tracks the two runtime-side work items:

- **HAR-001**: make the existing remote sandbox gateway deployable as a Linux
  service.
- **HAR-002**: add an authenticated, mobile-compatible agent control service that
  injects the remote sandbox into the agent runtime.

## Current capabilities

The runtime already has several of the required primitives:

- `llm-harness-sandbox-remote` provides the HTTPS/Bearer/SSE remote sandbox
  client and adapts it to `Sandbox` / `ExecutionEnv`.
- `llm-harness-sandbox-gateway` provides an authenticated Linux gateway with
  Bwrap and Firecracker backend selection, persistent sandbox records, and
  restart recovery.
- `HarnessBuilder::build(env)` accepts an arbitrary `ExecutionEnv`, so a remote
  sandbox can be injected into the agent runtime.
- `AgentHarness` exposes prompt, steer, abort, state, usage, and a broadcast
  event stream.

What is still missing is the deployable service boundary between the phone and
the agent runtime:

- The gateway is currently exercised through library APIs and tests rather than
  an operator-friendly Linux service.
- There is no authenticated control plane that can create an agent session,
  route prompts and events, and bind the agent to a remote sandbox.
- The existing Inspector is a localhost debugging surface, not a production
  mobile control plane.

## Goals

- A phone client can authenticate and start an agent session backed by a remote
  Linux sandbox.
- Prompt submission, streamed events, steering, and abort work over HTTPS.
- The agent runtime can use `RemoteSandbox` / `RemoteEnv` without special-casing
  mobile clients.
- Sandbox creation, execution, cancellation, and cleanup follow the existing
  timeout, output-limit, tenant, and fail-closed policies.
- The gateway can be deployed on a Linux host behind a reverse proxy or tunnel
  without embedding it into tests.
- The design can be tested with `curl` or a small script before any mobile UI
  work begins.

## Non-goals for the MVP

- Native mobile application UI, voice input, push notifications, or app-store
  packaging.
- Running the agent runtime process on the phone.
- Production validation of Firecracker; Bwrap is the default backend.
- Complete production audit, token rotation, network allowlists, disk quota, or
  cgroup deployment validation.
- Multi-node scheduling, distributed session storage, or horizontal scaling.
- Replacing the existing Inspector with a general-purpose admin product.

These limits do not make the design incompatible with them later. They only
prevent the first implementation from becoming a full mobile product and
production control plane at the same time.

## MVP topology

```text
phone client (HAR-003, out of scope here)
   |
   | HTTPS + bearer + JSON/SSE
   v
agent control service (HAR-002)
   |- session registry
   |- agent runtime manager
   |- event fanout
   |- remote sandbox handle
   |
   | existing remote sandbox protocol
   v
remote sandbox gateway (HAR-001)
   |- BwrapSandbox (default)
   |- FirecrackerSandbox (explicit opt-in)
```

The control service and sandbox gateway may run in the same process for local
development, but they are separate services in the target deployment. Keeping
them separate has two benefits:

1. The sandbox gateway can enforce tenant isolation without embedding agent
   session semantics.
2. The mobile control plane can evolve independently of the sandbox protocol.

The control service is the only component that talks to the sandbox gateway on
behalf of the phone. The phone never receives gateway host paths, gateway
credentials, or direct gateway URLs.

## New service boundary

### `llm-harness-agent-control`

Add a new small service crate for the mobile-compatible control plane. It should
own:

- authenticated HTTP routes;
- agent session metadata;
- mapping from session id to sandbox id and `AgentHarness`;
- background prompt execution;
- event fanout to SSE clients;
- abort and shutdown coordination.

It should not own:

- sandbox isolation policy;
- Firecracker lifecycle details;
- remote protocol parsing beyond using `RemoteSandboxClient`;
- mobile UI state management.

The crate should stay deliberately small. Most agent semantics should remain in
`llm-harness-agent`; most sandbox semantics should remain in
`llm-harness-sandbox-gateway` and `llm-harness-sandbox-remote`.

## Gateway deployment shell

### Configuration

HAR-001 should add a runnable gateway binary with explicit configuration for:

- bind address;
- work root;
- one or more principals and bearer-token hashes;
- backend selection: Bwrap or Firecracker;
- execution and request limits;
- logging level.

A simple TOML or environment-variable configuration is sufficient. The exact
format should prioritize reproducibility and avoid leaking secrets into process
arguments or logs.

### Runtime behavior

The gateway process should:

- bind only to an explicitly configured address;
- expose readiness and health endpoints;
- initialize the existing persistent registry;
- shut down gracefully on SIGTERM/Ctrl-C;
- stop or reconcile its sandboxes according to the existing gateway recovery
  semantics;
- log structured lifecycle events.

### TLS

The MVP should not implement TLS inside the gateway process. Instead:

- local development may use loopback HTTP;
- production deployment terminates TLS with Caddy, Nginx, a sidecar, or a
  tunnel such as Cloudflare Tunnel or Tailscale Funnel;
- the control service must configure the remote client with HTTPS for
  non-loopback gateway endpoints; the existing remote-client policy already
  rejects plain HTTP there.

This keeps the service focused on sandbox execution and leaves transport policy
to the deployment environment.

## Agent control API

The MVP API should use JSON for control operations and SSE for streaming.

| Method and path | Success | Semantics |
|---|---|---|
| `POST /v1/agent-sessions` | `201` | Create an agent session and its remote sandbox |
| `GET /v1/agent-sessions/{id}` | `200` | Return session status, sandbox status, and metadata |
| `POST /v1/agent-sessions/{id}/prompt` | `202` | Start an agent turn asynchronously |
| `POST /v1/agent-sessions/{id}/steer` | `202` | Inject guidance into the current or next turn |
| `POST /v1/agent-sessions/{id}/abort` | `204` | Abort the current turn |
| `GET /v1/agent-sessions/{id}/events` | `200` + SSE | Stream agent events |
| `DELETE /v1/agent-sessions/{id}` | `204` | Stop the agent and delete its sandbox |

### Create session request

A minimal create request should include:

```json
{
  "model": "claude-3-5-sonnet",
  "system_prompt": "optional",
  "sandbox": {
    "backend": "bwrap"
  }
}
```

The exact configuration schema can be refined during implementation, but the
first version should support:

- model selection;
- optional system prompt;
- sandbox backend selection;
- optional execution timeout;
- optional output limit.

The control service should reject unknown or unsafe fields rather than silently
downgrading them.

### Prompt semantics

`prompt` should return `202 Accepted` immediately. The service then invokes
`AgentHarness::prompt` in the background. This avoids holding an HTTP request
open while the agent runs.

The response should include a stable prompt or turn id so the client can
correlate subsequent events.

### Event stream

The event stream should wrap existing `AgentHarnessEvent` values in a stable
JSON envelope:

```json
{
  "type": "text_delta",
  "session_id": "...",
  "turn_id": "...",
  "timestamp": "...",
  "data": {}
}
```

The first version should forward:

- text deltas;
- tool-call lifecycle events;
- phase changes;
- errors;
- turn completion.

The stream should be readable by a browser `EventSource`, a native HTTP client,
or `curl`. Reconnection can initially be based on session status and the current
event buffer; durable replay is not required for MVP.

### Abort semantics

`abort` should cancel the current agent turn but not necessarily delete the
sandbox. The control service should:

- call `AgentHarness::abort`;
- emit an abort event;
- leave the sandbox available for the next prompt;
- clean up any orphaned execution through the existing remote protocol.

## Session lifecycle

### States

The control service should track at least:

| State | Meaning |
|---|---|
| `Creating` | Sandbox or agent runtime is starting |
| `Ready` | Agent is ready to accept a prompt |
| `Running` | A prompt is active |
| `Stopping` | Abort, shutdown, or cleanup is in progress |
| `Failed` | The session cannot continue |
| `Deleted` | The session and sandbox have been cleaned up |

The mobile client should see only these control-plane states. It should not need
to understand gateway-internal sandbox state.

### Ownership

Each session belongs to one principal. The control service should enforce:

- only the owning principal can read, prompt, steer, abort, stream, or delete a
  session;
- every session maps to one sandbox record;
- sandbox credentials are never returned to the phone;
- per-principal session limits are configurable.

### Cleanup

Session deletion should stop the agent first, then delete the sandbox through the
remote protocol. Cleanup should be idempotent.

If any part of cleanup fails, the service should retain enough metadata to retry
or report the orphaned resource. It must not silently forget a live sandbox.

### Restart behavior

The MVP does not need to restore in-memory `AgentHarness` state. On control-service
restart:

1. load persisted session metadata;
2. identify sessions that cannot be restored;
3. mark them `Failed`;
4. delete their remote sandboxes where possible;
5. report any unrecoverable orphans.

Durable conversation replay and live agent migration are later work.

## Security model

### Authentication

All mobile control endpoints require a bearer token. For MVP:

- a principal may be represented by a configured user token;
- the control service verifies the principal;
- the control service uses a separate service credential to talk to the sandbox
  gateway;
- the phone never receives the sandbox gateway token.
- model-provider credentials remain server-side only and never enter sandbox
  workspaces or phone-visible responses.

The control service should reuse the same general bearer-token discipline as the
sandbox gateway: no whitespace or control characters, no credential logging, and
no credential leakage through error messages.

### Authorization

The MVP should support at least one principal, but authorization must not be
accidentally omitted:

- sessions are scoped by principal;
- sandbox creation is performed on behalf of the principal;
- admin or cross-principal access is not part of the MVP;
- per-principal session and request limits should be enforced.

### Transport

- loopback HTTP is allowed for development;
- non-loopback clients must use HTTPS;
- TLS termination belongs to the reverse proxy or tunnel;
- the control service should not follow redirects for authenticated requests;
- SSE and control responses should not cache credentials or host paths.

## Sandbox policy

Bwrap is the default backend for MVP. It provides namespace and capability
isolation and is already supported by the gateway.

The default sandbox should be restrictive:

- filesystem access remains anchored to the guest workspace;
- networking is denied by default;
- unsupported policy fields fail closed;
- shell execution has timeout and output limits;
- every sandbox has an explicit owner.

Firecracker can remain an explicit opt-in backend. The MVP should not claim
production VM isolation until a real Linux/KVM deployment validates it.

## Observability

The first version should provide:

- structured logs for session create, prompt, steer, abort, delete, and cleanup;
- `session_id`, `principal`, `sandbox_id`, and `request_id` fields;
- lifecycle logs for control-service and gateway startup/shutdown;
- no bearer tokens, model credentials, or raw prompt content in logs;
- basic counters for active sessions, active prompts, and cleanup failures.

OpenTelemetry and immutable audit logs are valuable but not required for the
first mobile MVP.

## Testing plan

### Unit tests

- configuration parsing and validation;
- session state transitions;
- principal ownership checks;
- cleanup and restart reconciliation.

### Service integration tests

Using an in-process gateway or a fake remote sandbox:

- create session;
- submit prompt;
- receive streamed events;
- steer during a run;
- abort a run;
- delete session and confirm sandbox deletion;
- reject access from a different principal;
- reject unsupported sandbox policies.

### Gateway service tests

- start from TOML/env configuration;
- expose readiness;
- accept the existing remote protocol;
- recover registry state after restart;
- shut down without leaking child processes.

### End-to-end smoke

Before building a mobile UI, a scripted smoke test should verify:

```bash
create session -> prompt -> stream events -> steer -> abort -> delete session
```

This can be implemented with `curl` and is sufficient to validate the backend
contract.

## Acceptance criteria

HAR-001 is done when:

- [ ] the sandbox gateway can be started from explicit configuration;
- [ ] health/readiness and graceful shutdown are implemented;
- [ ] Bwrap is the default tested backend;
- [ ] deployment docs show a reverse-proxy or tunnel setup;
- [ ] no sandbox resources leak after normal shutdown.

HAR-002 is done when:

- [ ] the control service can create an agent session backed by `RemoteSandbox`;
- [ ] prompt, event streaming, steer, and abort work end to end;
- [ ] sessions are scoped to an authenticated principal;
- [ ] session deletion stops the agent and deletes the sandbox;
- [ ] restart reconciliation does not leave unreachable live sandboxes;
- [ ] integration tests cover the mobile control contract.

The combined MVP is usable when a scripted client can complete the full flow:

```text
authenticate -> create session -> prompt -> receive events -> abort -> delete
```

## Later work

After the backend MVP:

- build the mobile client or mobile web UI;
- add durable conversation history and event replay;
- add push notifications for long-running turns;
- integrate immutable audit logging;
- add token rotation and revocation;
- implement network allowlists and disk quotas;
- validate Firecracker on real Linux/KVM hosts;
- add production metrics, alerting, and runbooks.
