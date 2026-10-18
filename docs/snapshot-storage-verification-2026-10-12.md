# Snapshot storage implementation and verification — 2026-10-12

Implemented in the existing working checkout of `llm-harness-runtime`; HEAD remained
`ee3274c5e9cb66332ccb40afe35967e153af4454`. No Runtime repository commit was made.
The checkout already contained substantial uncommitted V0 workflow changes,
including the untracked `pipeline.rs` implementation. Those changes were retained.
No Blender project or existing real Snapshot evidence was used as cleanup data.

## Architecture findings and decisions

| Area | Finding / implementation point |
| --- | --- |
| Capture | `src/pipeline.rs::ArtifactSnapshot` enumerates the existing Git-aware or filesystem policy, buffers bounded source bytes, hashes them, writes captured files, captures Git state, verifies r0, builds task-relative binary patches, and publishes the manifest last. |
| Historical storage | `.pipeline/snapshots/<run UUID>/r<round>`; no previous retention lifecycle. rN references the same run's immutable r0. Removing individual rounds breaks provenance. |
| Actual configuration | `Settings::load/save` uses Studio `settings.json`. The legacy hand-rolled TOML parser is not the running Studio configuration entry point. Reused JSON instead of adding another file/layer. |
| Workflow lifecycle | `PipelineController::run` owns decisions/review; `TeamEntry::start_workflow` owns worker cancellation and quiescence. Completion eligibility is recorded only after that owner observes Idle workers. |
| Existing observability | Reused the controller sink, EventBus, and `tracing`; durable operation JSON belongs to the existing `.pipeline` evidence storage. |
| Protection | Minimal versioned per-run JSON, explicit reasons and dependencies, conservative legacy/unknown treatment, existing manifest baseline references, and OS shared/exclusive leases. No database/service/Agent/backend was added. |
| Cleanup unit | Complete run. This keeps every retained r0→rN chain intact. Known external run dependencies conservatively protect their targets, even if grouping dependent runs might reclaim more. |

Defaults remain 10,000 files, 16 MiB/file, 128 MiB aggregate; automatic retention
is disabled. Supported explicit aggregate allowance is capped at 512 MiB. Every
new manifest records the actual capture policy. Allocation-heavy aggregate
hashing no longer constructs a second copy of each file's contents.

See [configuration and operation instructions](snapshot-storage.md) for all exact
keys, bounds, loading behavior, protection commands, dry run, and record locations.

## Files changed for this task

- `crates/llm-harness-agent-team-studio/src/pipeline.rs`: configurable capture APIs,
  effective-limit manifest field, unchanged hash encoding, workflow lifecycle,
  asynchronous admission when maintenance owns the lease, and retention event.
- `crates/llm-harness-agent-team-studio/src/snapshot_storage.rs`: new validated
  policy, locks, completion/protection/dependencies, allocated-storage inventory,
  complete-run hysteresis, durable deletion intents, recovery, and synthetic tests.
- `crates/llm-harness-agent-team-studio/src/bin/snapshot-storage.rs`: maintenance
  CLI for preview/cleanup, pin/unpin, dependency registration, and leased inspection.
- `crates/llm-harness-agent-team-studio/src/settings.rs`: validates and preserves
  the Snapshot object in the existing settings document.
- `crates/llm-harness-agent-team-studio/src/team_manager.rs`: loads policy, holds
  evidence through worker shutdown, marks safely completed runs, invokes cleanup.
- `crates/llm-harness-agent-team-studio/src/lib.rs`: exports the storage module.
- `crates/llm-harness-agent-team-studio/Cargo.toml`: enables existing `uuid` serde
  support and Tokio dev `test-util` required by paused-time workflow tests.
- `crates/llm-harness-agent-team-studio/tests/integration.rs`: automatic retention
  through production team construction, 3→1 low-watermark cleanup, configured
  manifest limits, event correlation, and synthetic model/workspace preservation.
- `crates/llm-harness-agent-team-studio/tests/decision_protocol.rs`: isolates the
  timeout fixture in its own temporary directory instead of capturing all `/tmp`.
- `docs/snapshot-storage.md` and this report: operation, safety, resource evidence,
  configuration, verification, and current limitations.

## Test evidence

Validation ran on Linux with Rust 1.98.1. Every destructive test uses uniquely
created synthetic temporary Snapshot storage. Resource fixtures use generated
private directories on the normal project filesystem, avoiding `/tmp` tmpfs
capacity pressure, and remove their own fixture directories after completion.

| Check | Result |
| --- | --- |
| `cargo check -p llm-harness-agent-team-studio --all-targets` | Passed; existing unused-variable/dead-code warnings remain. |
| New `snapshot_storage::tests` in final library run | 19 passed; two resource probes normally ignored and executed explicitly below. |
| Existing `pipeline::tests` in final library run | All 63 passed, including ignores/exclusions, symlinks, immutable round recovery, binary/task-relative provenance, and failed-capture dispatch gates. |
| `cargo test -p llm-harness-agent-team-studio --lib` | **236 passed, 2 failed, 2 ignored**. Two unrelated team-assembly failures are detailed below; the broad library suite is not claimed green. |
| `--test decision_protocol` | 8 passed. |
| `--test escalate_workflow` | 3 passed. |
| `--test eval_isolation` | 8 passed. |
| `--test integration` | 23 passed; 4 existing live-LLM tests ignored (no live endpoint runs). |
| Two explicit generated-data resource probes | Both passed; measurements below. |
| Focused `git diff --check` | Passed. |
| Read-only existing evidence SHA-256 comparison | 9,764 files checked; **0 missing, 0 changed**. |

Commands for the integration group:

```sh
cargo test -p llm-harness-agent-team-studio --test integration \
  --test decision_protocol --test escalate_workflow --test eval_isolation
cargo test -p llm-harness-agent-team-studio snapshot_storage::tests --lib
cargo test -p llm-harness-agent-team-studio pipeline::tests --lib
```

The final new tests cover configured file count/per-file/aggregate limits;
defaults and invalid config; disabled retention; 79/90/99/100/101/120 boundaries;
exact 101→80; storage pressure and combined pressure; active/pinned/legacy/E2E
protection; explicit and legacy baseline dependencies; contiguous whole chains;
idempotence; real capture alongside attempted cleanup; another-process Reviewer
lease; maintenance-first workflow admission; dry-run immutability; insufficient
reclaimable capacity; partially deleted detached-trash recovery; unknown/orphan
trash; damaged manifests/metadata; unknown metadata versions; and symlinks in both
cleanup storage and protection-operation parent directories.

One parallel test initially exposed a lock lifetime issue: another thread's
fork can briefly inherit a shared-lock file descriptor before CLOEXEC. Explicit
unlock on lease drop fixed it; parallel tests and a separate-process lease test
pass. A snapshot symlink regression initially showed premature directory creation;
validating initial capture inputs before creating new storage restored the original
failure behavior. These test findings informed the final implementation.

Two remaining broad library failures concern behavior present in the pre-task
team-assembly implementation, outside Snapshot storage:

1. `team_manager::tests::invalid_spec_does_not_create_team_directory`: the existing
   sandbox preparation creates `teams/.../issues` before assembly validates an
   invalid team ID. The test expects no `teams` directory.
2. `team_manager::tests::persisted_team_restarts_with_issues_contacts_and_runner`:
   the existing sandbox setup persists `sandbox_mode=Some("bwrap")`, whereas the
   test compares the entire restored spec to a fixture with `sandbox_mode=None`.

They were left unchanged to avoid modifying unrelated Runtime/Studio behavior.
No pre-task complete-suite baseline was executed; the attribution above follows
inspection of unchanged assembly code and the recorded initial working state.

## Resource experiments

Run ignored probes directly using the built test executable to measure its RSS,
not Cargo's compiler RSS:

```sh
cargo test -p llm-harness-agent-team-studio --lib --no-run
# Substitute the test executable printed by Cargo for TEST_EXECUTABLE.
/usr/bin/time -v TEST_EXECUTABLE \
  snapshot_storage::tests::large_capture_resource_probe \
  --exact --ignored --nocapture
/usr/bin/time -v TEST_EXECUTABLE \
  snapshot_storage::tests::incompressible_revision_resource_probe \
  --exact --ignored --nocapture
```

| Generated workload | Outcome | Peak RSS | Wall time | Evidence overhead |
| --- | --- | --- | --- | --- |
| 32 × 16 MiB files; r0 plus r1 with a small binary change | Default 128 MiB rejected; configured 512 MiB accepted; baseline verified and task-relative patch generated | 575,712 KiB (562.2 MiB) | 50.90 s | Roughly 1 GiB of retained rounds |
| 65 × 2 MiB incompressible files; all files rewritten for r1 | Default 128 MiB rejected; configured 512 MiB accepted; all 65 changed paths verified | 504,720 KiB (492.9 MiB) | 29.32 s | Binary patch 351,394,290 bytes (335.1 MiB); roughly 595 MiB of round/patch evidence |

These debug-build runs establish that explicitly larger captures work and show
why aggregate payload size is not the process memory bound. They do not establish
safe memory usage for fully changed incompressible 512 MiB revisions. Git patches
and temporary object storage can dominate that case. Streaming capture/patches,
separate patch quotas, and formal process-wide memory limits remain future work.

## Existing evidence preservation

Before implementation, a read-only script streamed SHA-256 over the existing
files under top-level project `.pipeline/snapshots` trees into
`/tmp/snapshot-retention-before.json`. After all destructive fixture tests and
resource experiments, it reread every recorded path and compared hashes:

```json
{"checked": 9764, "missing": [], "changed": []}
```

The result is saved in `/tmp/snapshot-retention-after.json`. The real Blender
history was included in this preservation audit. No cleanup command, retention
adoption, archival, metadata rewrite, or destructive experiment was run against
those real trees. New legacy-safe retention does not make old histories eligible.
Raw session check/test/resource logs are in `/tmp/snapshot-*.log`; the durable
results are summarized here rather than introducing another logging subsystem.

## Current safety limits

Only cooperating new Runtime/inspection processes are covered by advisory locks.
External collectors must lease their copy operation, and benchmarks/experiments
must declare protection before the run or pin completed evidence. Unknown legacy,
failed, and interrupted histories remain protected indefinitely. Metadata migration,
automatic external dependency discovery, background periodic maintenance, journal
pruning, full-content rehashing during cleanup, and Windows-specific validation
are not implemented. Deleted runs' historical terminal packages retain their old
links; operation records identify the expired run and removed paths, while all
retained Snapshot baseline chains remain complete.
