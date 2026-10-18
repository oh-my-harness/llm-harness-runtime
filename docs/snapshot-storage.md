# Snapshot capture capacity and retention

The V0 coding workflow captures immutable r0 task-start evidence and subsequent
review rounds in `<workspace>/.pipeline/snapshots/<run UUID>/r<round>/`.
`pipeline.rs::ArtifactSnapshot` owns capture; `PipelineController` owns the
workflow and Reviewer dispatch; `TeamEntry::start_workflow` owns worker shutdown.
Capture publishes `snapshot.json` last. A capture error prevents Reviewer dispatch.

Git captures continue using `git ls-files` with the existing ignore handling,
tracked files, ordinary untracked files, and `.git`/`.pipeline` exclusions.
Outside Git, the existing filesystem fallback rejects `.gitignore` rules and
symlinks. No ignore policy has been redesigned. The configured file-count bound
also applies to the Git included/ignored path inventories, as the old bound did.

There was no historical retention or durable run completion/protection metadata.
Individual round deletion would invalidate r0 baselines and revision histories.
Retention therefore removes a **complete run**, including all its rounds, files,
manifests, and patches. References between retained evidence are protected.
Expired runs are identified in durable cleanup records; their old terminal chat
packages remain historical records, and their former file links expire.

## Configuration

Reuse the existing global Studio `<data_root>/settings.json` document. The default
root is `$HOME/.harness/agent-team-studio`, overridden by `STUDIO_DATA_ROOT`, then
`CT_DATA_ROOT`, or the embedding runtime's explicit data root. Preserve the other
settings keys when adding this object:

```json
{
  "snapshots": {
    "capture": {
      "max_files": 10000,
      "max_file_bytes": 16777216,
      "max_total_bytes": 536870912
    },
    "retention": {
      "enabled": true,
      "count_high": 100,
      "count_low": 80,
      "bytes_high": 10737418240,
      "bytes_low": 7516192768
    },
    "protect_new_runs": []
  }
}
```

This example explicitly enables retention and raises aggregate capture to
512 MiB for artifact-heavy projects. Neither change is enabled by default.

| Setting | Default | Supported values |
| --- | --- | --- |
| `capture.max_files` | 10,000 | 1–100,000 |
| `capture.max_file_bytes` | 16 MiB (16,777,216) | 1–64 MiB |
| `capture.max_total_bytes` | 128 MiB (134,217,728) | 1–512 MiB |
| `retention.enabled` | `false` | Boolean |
| `retention.count_high` / `count_low` | 100 / 80 | Positive low < high; high <= 1,000,000 |
| `retention.bytes_high` / `bytes_low` | 10 GiB / 7 GiB | Positive low < high; high <= 1 PiB |
| `protect_new_runs` | `[]` | Up to 100 nonempty protection reasons, each <= 1,024 bytes |

Omitted fields use defaults. Invalid types, unknown keys inside `snapshots`,
nonpositive bounds, unsupported resource limits, and invalid watermarks are
rejected. They cannot silently disable protection. Studio startup validates the
object; team creation validates and loads it again before assembly. Model/settings
saves preserve it. Restart Studio or recreate the team after editing it: an
existing controller retains its policy. This is global configuration, not a new
per-workflow or per-team configuration layer. Embedded callers can supply a
validated `SnapshotPolicy` using `PipelineController::with_snapshot_policy`, or
call `ArtifactSnapshot::collect_configured`. The old `collect` API uses defaults.
Each new manifest records its effective `capture_limits`; older manifests load
compatibly with the historical defaults.

## Trigger and accounting

Automatic retention runs at the end of a controlled workflow, after the controller
closes its gate, workers receive cancellation, and worker Idle states are checked.
A successful terminal FINISH or explicit ESCALATE can become ordinary completed
history. Capture/decision errors, cancellation, crashes, or workers that do not
settle within five seconds leave the run active/unknown and protected.

If count **or** storage reaches its high watermark, select eligible oldest runs
by durable creation time until **both** low targets are satisfied. Complete-run
removal can undershoot a target. Count includes all retained manifests, including
protected and legacy runs. On Unix, storage uses allocated blocks (including run
subdirectories, manifests, patches, and lifecycle metadata), not just source
payload lengths. On other platforms it uses logical lengths; Windows behavior has not been tested
in this Linux validation run. Journals, temporary
Git patch repositories, and other project files are outside this managed usage.
This is a retained-storage policy, not a filesystem free-space quota.

With eligible single-round runs, 79/90/99 under a 100/80 policy do nothing;
100/101/120 become 80. With three-round runs, removal always keeps or removes the
whole chain. Protected history can prevent either target: the report marks
`incomplete` and lists skipped runs rather than deleting them.

## Protection and inspection

`retention.json` in each newly managed run records a schema version, UUID,
creation/completion times, protection reasons, and historical dependencies.
Missing, invalid, unknown-version, or inconsistent metadata never grants deletion
eligibility. Old real histories are not adopted or rewritten. A damaged retained
manifest or unsafe filesystem entry aborts cleanup and produces an error record.

Pin an entire completed run for an E2E baseline, reproducible experiment, or
important result. In an isolated benchmark/experiment runtime, set
`"protect_new_runs": ["E2E benchmark"]` **before starting the workflow** to protect
all new runs from their creation. Protection is always honored; there is no
configuration that overrides it. External benchmark requirements cannot be
inferred from arbitrary external files or consumers, so declare them explicitly.

Build/use the existing workspace's new maintenance binary:

```sh
cargo build -p llm-harness-agent-team-studio --bin snapshot-storage

target/debug/snapshot-storage dry-run /path/to/workspace /path/to/studio-data-root
target/debug/snapshot-storage cleanup /path/to/workspace /path/to/studio-data-root

target/debug/snapshot-storage pin /path/to/workspace RUN_UUID "E2E baseline"
target/debug/snapshot-storage unpin /path/to/workspace RUN_UUID "E2E baseline"
target/debug/snapshot-storage depend /path/to/workspace OWNER_UUID TARGET_UUID
target/debug/snapshot-storage undepend /path/to/workspace OWNER_UUID TARGET_UUID

target/debug/snapshot-storage inspect /path/to/workspace RUN_UUID
```

`inspect` holds a shared inspection lease until Enter/EOF. Hold it in another
terminal for the entire manual review or external collector copy. Library callers
can hold `StorageLease::inspection`. All built-in workflow review cycles hold this
lease automatically. Pin/dependency mutations use the same exclusive lock as
cleanup; they report busy while a workflow or inspection is active. Pin before
inspection, or use `protect_new_runs` to designate a running experiment.
Do not unlink or replace `snapshot-storage.lock`.

Dry run writes an audit record but does not detach/delete snapshots or recover
trash. Its `after` usage is the projected result; `reclaimed_bytes` remains zero.
Explicit cleanup still respects `enabled=false`. Exit code 2 indicates incomplete
cleanup or a recorded error; lock/configuration/I/O errors exit unsuccessfully.
A busy cleanup is deferred, logged, and retried at a later workflow completion or
manual invocation. There is no background daemon or periodic polling.

## Safety, recovery, and records

Shared OS file locks cover captures and entire workflow/Reviewer lifetimes,
including the interval between terminal reporting and worker shutdown. An
exclusive nonblocking lock covers selection, deletion, recovery, pins, and
explicit dependency updates across threads and processes. Workflow admission waits
asynchronously if maintenance acquired its lease first, avoiding a transient
cleanup conflict becoming a task failure. Completion updates
hold a shared lease and occur only after quiescence. Kernel release on process
exit avoids stale lockfiles; explicit unlock handles inherited descriptors during
concurrent process spawning. On a new workspace, capture input validation can
fail before creating storage, preserving the old failure behavior.

Selection verifies a contiguous r0→rN chain, matching manifest identities and
locations, same-run r0 baselines, and the recorded file inventory. Known historical
references protect their targets, including references found in legacy manifests.
Active and pinned runs are always skipped. Metadata does not authorize changes to
the active workspace, Git history, source files, models, or current renders.

Each operation writes `.pipeline/snapshot-retention/<operation UUID>.json`, uses
existing `tracing`, and automatic operations emit a `snapshot_retention` event
through the existing workflow sink/EventBus with project and issue correlation.
Records include thresholds, triggers, before/after usage, selected UUIDs/counts/
sizes, skipped protections, archived/removed paths, reclaimed bytes, errors, and completion
phase. `usage_known=false` distinguishes an aborted inventory from zero usage.
If audit storage itself fails, cleanup cannot proceed without a durable intent.

Deletion first fsyncs a versioned intent, then atomically moves a run into
`.pipeline/snapshot-trash/<operation UUID>/<run UUID>/`, fsyncs the affected
directories, removes that detached tree, and persists progress. A later enabled
cleanup validates the intent and recovers only already detached trash, including
partially deleted trees. It does not resume deleting source runs from an old plan.
Unknown/orphaned trash is preserved and reported for inspection. Disabled
retention and dry runs never recover/delete trash. Records are an audit trail and
interruption recovery mechanism, not a backup of deleted file contents.

## Resource evidence and limitations

Capture still keeps aggregate bytes in memory. Incremental aggregate hashing
removes the old extra per-file content copy while preserving hash encoding.
A synthetic 512 MiB r0+r1 capture with a small binary change completed at roughly
562 MiB peak RSS in a debug build, taking about 51 seconds and retaining about
1 GiB of round evidence. A 130 MiB incompressible corpus with all 65 files changed
produced a 335 MiB binary patch and reached roughly 493 MiB RSS in 29 seconds.
Both datasets exceeded the default aggregate bound and passed under 512 MiB.

A 512 MiB capture limit is not a total process-memory cap. Git status/diff output,
binary patches, manifests, allocator overhead, and temporary Git objects add
memory, disk, and latency. A fully changed incompressible 512 MiB corpus was not
measured and can require substantially more than 1 GiB of memory and temporary
storage. Choose the smallest adequate allowance; provision temporary storage for
the existing Git patch machinery. Streaming capture/patch output and independent
patch-size quotas are future work, not current capabilities.

Locks coordinate participating Runtime builds and inspection tools. Arbitrary
external readers, older Runtime builds that do not acquire leases, and hostile
filesystem writers cannot be coordinated by advisory locks. Do not run a legacy
writer or an unleased external inspection alongside enabled retention. Legacy,
failed, and interrupted runs remain protected indefinitely; safe migration and
manual adjudication tools are not implemented. Retention validates inventories
and references but does not rehash all retained bytes during every scan. Audit
records have no automatic pruning policy. No real Blender evidence is migrated,
rewritten, archived, or deleted by this implementation work.
