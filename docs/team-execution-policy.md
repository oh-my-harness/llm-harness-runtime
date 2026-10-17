# Team execution policy

Normal Teams use Bubblewrap by default. Failure to construct or start the
sandbox rejects creation/restoration; there is no automatic unconfined fallback.
`team.json`'s `sandbox_mode` reports the applied backend (`bwrap` or
`trusted-host`). Editing it cannot authorize host execution.

## Operator authorization

An operator can explicitly authorize an existing development Team to reuse
`OsEnv`. This is **unrestricted host execution under the Runtime user's OS
identity**, including filesystem, network, environment/credentials, and Windows
interop when launched from WSL. It is not an executable allowlist, workspace
confinement, or a Blender-specific permission. Authorize only trusted Teams,
inputs, and scripts. Every member's execution environment receives this grant;
existing workflow and Reviewer tool guards still apply.

Build the updated Runtime and operator CLI from the repository:

```sh
cargo build -p llm-harness-agent-team-studio --bin agent-studio --bin team-execution
```

For the existing BlenderGundam Team:

```sh
./target/debug/team-execution authorize \
  /home/liyufeng/.harness/agent-team-studio BlenderGundam \
  --acknowledge-unrestricted-host-access
```

This command records an explicit operator grant; it does not execute a workflow.
The workspace must already have a normal workspace grant. The CLI resolves the
persisted Team's workspace to its canonical path and binds authorization to the
exact Team ID and path. Changing the workspace invalidates the grant. Deleting
the Team removes its grant. Normal workspace browsing grants alone do not
authorize host execution.

The CLI stores grants in `trusted-host-grants.json` under the Runtime data root,
using atomic private-file writes (0600 on Unix). Runtime rejects symlinked,
non-private, or differently owned stores. A Team workspace cannot contain the
Runtime data root, preventing sandboxed agents from editing the policy store.
No agent tool or HTTP authorization endpoint is added. The grant is independent
of templates, model output, and `sandbox_mode`.

Apply the grant only when the Team is idle: load the rebuilt Runtime and restart
the Team through the existing `POST /api/team/projects/restart?id=BlenderGundam`
operator endpoint (using the current panel Bearer token), or stop/start Runtime.
Team restart rebuilds its execution environment without submitting a new task;
do not restart an active Team, since that interrupts its current turn.
Inspect the persisted `sandbox_mode` afterward; it should be `trusted-host`.
The Runtime process itself must run in the original WSL host environment so
`OsEnv` inherits working WSL mounts and interop. An outer container/sandbox is
not removed by this policy.

List or revoke grants:

```sh
./target/debug/team-execution list /home/liyufeng/.harness/agent-team-studio
./target/debug/team-execution revoke /home/liyufeng/.harness/agent-team-studio BlenderGundam
```

Grants are checked during Team construction, including Team restart and Runtime
restoration. Revocation is not an interruption mechanism: **stop/restart an
already running host Team immediately after revocation**. Existing processes
retain their backend until stopped. Restored Teams without a matching grant use
Bubblewrap. Malformed/insecure grant stores fail closed during development Team
creation/restoration rather than silently changing privileges.

## Evaluation boundary

The E2E launcher starts its separate Runtime process with `STUDIO_EVALUATION=1`.
Presence of this variable (even an empty value) permanently selects evaluation
mode for that manager's lifetime. Evaluation ignores all host grants, regardless
of Team ID, workspace, template, or persisted `sandbox_mode`, and requires a
successfully started Bubblewrap sandbox. There is no Runtime API for disabling
this mode. An agent's child-shell environment cannot alter its parent manager.

Before registering/submitting work, the evaluator checks the authenticated
`GET /api/team/startup` response for `execution_mode: "evaluation"`. It also
retains the `sandbox_mode == "bwrap"` admission check. Old/development runtimes
are rejected before agent execution. The existing isolated per-attempt data
root, workspace, acceptance runner, and hidden evaluator state remain separate.
Do not launch evaluations against an ordinary development Runtime. Anyone
implementing a separate evaluator launcher must set this variable and perform
the same admission check.

Sandboxed agents cannot reach host grants, hidden acceptance files, or the
host execution CLI outside their mounts. Granting a trusted development Team
does not create a host execution endpoint reachable from evaluation sandboxes.
Trusted host Teams have the user's authority and can read same-user evaluator
files; this policy does not isolate private data from deliberately trusted host
processes. They can also modify same-user Runtime state, including host grants;
the operator CLI is not an OS privilege boundary against an already authorized
host process. Initial authorization therefore requires trusting the entire Team
and the code it executes. Run adversarial/untrusted work only in
evaluation/sandboxed mode.

## Verification

The execution-policy regression tests use real `TeamManager` construction,
the production Coder harness, and its `bash` tool. Only LLM responses are mocked.
They cover default confinement, forged metadata, explicit authorization,
scope, revocation, Team restart, Runtime restoration, and evaluation overriding
an existing host grant. The WSL smoke test uses BlenderGundam's actual workspace
with isolated temporary Runtime state, runs `wslpath` and Blender `--version`,
and starts no Gundam workflow or Snapshot capture:

```sh
cargo test -p llm-harness-agent-team-studio --test execution_policy
cargo test -p llm-harness-agent-team-studio --test execution_policy \
  blendergundam_production_coder_launches_windows_blender -- --ignored --nocapture
```

Verified on October 13, 2026 (WSL Ubuntu): 288 Rust regressions passed across
Studio library, Coding Team integration, decision protocol, escalation,
evaluation isolation, and execution policy; seven optional tests were ignored.
The explicit Blender smoke test additionally passed and returned
`Blender 5.2.2 LTS` / `build platform: Windows`. All 72 evaluation Python tests
passed. Live LLM/provider tests were not run.

The Blender smoke required execution outside Codex's outer command sandbox:
inside that sandbox, WSL's vsock call failed with `socket failed 1`, despite
the Runtime selecting `trusted-host`. This confirms the documented distinction
between Runtime policy and any outer process confinement. Protected Gundam
file bytes were checked unchanged. The smoke authorized only temporary Runtime
state; it did not grant or restart the user's existing live Team.
