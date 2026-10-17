# P1 offline grader

`offline_grader.py` reads a preserved run or workspace directory without running
the Runtime, tests, Git, or a model. It uses only the Python standard library.

```sh
python3 tools/eval/offline_grader.py /path/to/run --output /path/to/separate-report
python3 -m unittest discover -s tools/eval -p 'test_*.py' -v
```

Call `grade(source)` for a deterministic Python dictionary. Call
`write_report(source, output)` to write `result.json` and `report.md`; output
must be outside source. JSON schema version `1`, grader version `p1.1`.
Evidence references contain a source-relative path and SHA-256; JSONL references
also contain a 1-based line number. Values appear only for `measured` metrics.
Other statuses are `blocked`, `not_measured`, `not_applicable`, and
`measurement_error`.

Snapshot hashes follow `pipeline.rs`: sorted paths, SHA-256 of captured file
bytes, and aggregate SHA-256 of each `path-byte-length US path US file-byte-length
RS bytes` record. The grader checks r0 relationships when present. It does not
replay `task-relative.patch`, execute acceptance tests, or assume a missing r2 is
a failed revision. Event JSONL is treated as an external capture, not a guaranteed
complete durable Runtime log.
`event_jsonl_parse` checks syntax; `event_stream_integrity` remains
`not_measured` because valid start and terminal rows do not prove that no
intermediate WebSocket events were lost. Older empty manifests lacking
`file_hashes` are recorded as `unavailable`, while corrupt modern manifests
are `malformed`.

Optional `evidence/independent-test.json` is accepted for code correctness only
when it contains `snapshot_id` for a valid r1-or-later capture, `suite_path`
relative to the source, matching `suite_digest` (SHA-256), nonempty `command`,
and integer `exit_code`. The caller is responsible for independently executing
the test against that exact captured snapshot and recording this claim. An
`independent-test.txt` alone remains unattributable and unscored.

## P2.5 independent acceptance runner

Create a fixed fixture outside the generated workspace. The suite must be a
single `test_acceptance.py` using Python's standard `unittest`; its assertions
import the captured project through `PYTHONPATH`. The declarative spec is JSON:

```json
{
  "fixture_id": "arithmetic-1",
  "fixture_version": "1",
  "fixture_path": "fixture.txt",
  "fixture_digest": "<SHA-256 of fixed task fixture>",
  "suite_version": "1",
  "suite_path": "test_acceptance.py",
  "suite_digest": "<SHA-256 of test_acceptance.py>",
  "timeout_seconds": 10
}
```

Pin the spec file's SHA-256 separately. Run against a preserved source with
Runtime `r1` and optional `r2` Snapshots:

```sh
python3 tools/eval/acceptance_runner.py /path/to/source \
  --spec /path/to/fixture/spec.json --spec-digest '<SHA-256 of spec.json>' \
  --output /path/to/new-acceptance --issue-id '<issue-id>'
python3 tools/eval/offline_grader.py /path/to/source \
  --acceptance-dir /path/to/new-acceptance --output /path/to/new-report
```

`run_acceptance(source, spec_path, output, issue_id=..., expected_spec_digest=...)`
is the reusable Python API. It verifies each manifest and captured file hash,
then copies exactly the manifest's listed files to a fresh disposable workspace.
The acceptance output, separate from historical source, contains pinned
`spec.json`, `fixture.txt`, `test_acceptance.py`, and `r1.json`/`r1.log`
(plus `r2` when present).
Records identify the Snapshot, manifest, suite, spec, command, exit code,
duration, test counts, backend, and error status. P1 verifies the records and
artifacts and reports `acceptance_test_counts`, `code_correctness`, and a
same-suite r1/r2 change in passed tests. Test errors, timeouts, missing isolation,
and invalid evidence do not become functional failures.

The default backend is bubblewrap with mount, PID, user, and network namespaces.
Only `/usr`, the disposable workspace, and the read-only suite are mounted;
the environment is cleared. CPU, address space, output file size, file
descriptors, process count (soft limit 512; Linux compares it against the
calling user's total process count, so a tighter value can make `unshare`
fail with `EAGAIN` on busy multi-process hosts), and wall time are limited.
If bubblewrap is missing
or cannot start, the result is `infrastructure_failure` and no code executes.
`--trusted-local` deliberately runs on the host for *trusted fixtures only*;
it keeps resource and wall-time limits but provides no filesystem or network
isolation. The runtime Snapshot's capture policy can exclude ignored files, so
the fixture must rely only on captured files. The suite and generated project
share a Python interpreter inside the sandbox, so do not place secrets in tests.

## P2 Studio collector

Start Studio as usual, then start the collector before submitting a Coding Team
prompt through the existing UI:

```sh
./scripts/studio.sh
python3 -B tools/eval/studio_collector.py \
  --data-root "$HOME/.harness/agent-team-studio" \
  --output "$HOME/studio-evaluations"
```

Wait for `Collector READY` before submitting the prompt. The collector reads the
private `panel.json` descriptor and uses its token only in the localhost
WebSocket/HTTP Authorization header. It does not print the token. It opens the
`/api/team/events?live_only=true` stream, whose readiness marker confirms that
the Runtime subscription exists. This mode does not change the ordinary Studio
event stream. The output root should be private because original task text and
captured source may be sensitive. Keep it outside every Coding Team workspace;
the CLI checks already registered workspaces before starting and skips a later
run if its workspace encloses the output root.

`pipeline_started` identifies a controlled workflow. `issue_id` on lifecycle
events and `broadcast_id` on chat events link it to the persisted terminal
message `<issue_id>:terminal`. The collector waits for that message in the
existing chat history API, copies only Snapshot directories whose manifest has
the same `issue_id`, then calls P1 `write_report`. It never starts a workflow.

Each run is saved under `OUTPUT/runs/<stable-id>/`:

```text
record.json                    collector state, gaps and optional identities
source/evidence/events.jsonl   original ordered WebSocket JSON messages
source/evidence/human-review-package.md
source/workspace/.pipeline/snapshots/<run-uuid>/r*/...
acceptance/spec.json, fixture.txt, test_acceptance.py, r1.json, r1.log (when associated)
result.json                    P1 result
report.md                      P1 report
```

An optional `--metadata-file` can contain project-scoped declarations:

```json
{"projects":{"my-team":{"fixture_id":"task-1","fixture_version":"1",
"fixture_digest":"...","baseline_digest":"...","suite_digest":"...",
"variant_id":"a"}}}
```

The six identity fields are retained. The run remains `ordinary` unless
all six are provided and the captured r0 `content_hash` matches
`baseline_digest`; then its mode is `controlled_metadata_only`. This records
identities without asserting test execution or rigorous comparability. To
automatically test after terminal Snapshot capture, also provide
`acceptance_spec` (absolute path) and `acceptance_spec_digest` (SHA-256). The
collector verifies the pinned spec's fixture identity and suite digest against
the six declarations, invokes P2.5, then passes the resulting acceptance
directory to P1. Without a verified association, Studio reports remain
descriptive and `code_correctness` stays unmeasured.

The collector resumes known unfinished runs after restart. It records a gap
when it loses the socket, restarts mid-run, misses `pipeline_started`, or the
Runtime broadcast channel reports dropped events. Missing events are not
silently backfilled from the 50-event EventBus replay buffer. A terminal
message may still permit descriptive grading with an incomplete event trail.
Runs already graded retain their original result and directory on repeated
terminal messages.

## P3 unattended sequential queue

Prepare a small Git repository as the immutable starting fixture. Its files
must be safe regular files; `.git` may be present, but symlinks are rejected.
The task prompt bytes must equal the acceptance spec's `fixture_path` bytes.
The queue pins copies and hashes of the fixture, prompt, acceptance files,
variant, and Coding Team template when a task is enqueued. For example,
`variant.json` may contain:

```json
{
  "id": "models-a",
  "models": {"strong": "model-a", "main": "model-b", "cheap": "model-c"},
  "base_url": "https://provider.example",
  "scout_interval_secs": 3600
}
```

Use a real credential file stored outside the queue. `prepare` and `enqueue`
are local and do not contact a provider. Build the current production Studio
binary before starting a live queue:

```sh
python3 tools/eval/e2e_queue.py prepare \
  --task-id task-1 --version 1 --fixture /path/to/fixture-git-repo \
  --prompt /path/to/fixture.txt --acceptance-spec /path/to/spec.json \
  --variant /path/to/variant.json --output /path/to/task-1.json
python3 tools/eval/e2e_queue.py enqueue --queue /path/to/private-queue /path/to/task-1.json
python3 tools/eval/e2e_queue.py list --queue /path/to/private-queue
cargo build --release -p llm-harness-agent-team-studio
python3 tools/eval/e2e_queue.py start --queue /path/to/private-queue \
  --studio-bin target/release/agent-studio --key-file /path/to/private-key \
  --max-attempts 2 --max-duration 3600 --attempt-timeout 1800 \
  --allow-provider provider.example --allow-model model-a \
  --allow-model model-b --allow-model model-c
```

Run `list` again to inspect state, attempt ID, workflow ID, classification,
error, and result path. `--max-attempts` counts queued tasks started in this
invocation. `--max-duration` and `--attempt-timeout` bound queue and attempt
execution. The scout interval must exceed the attempt deadline to avoid an
unrelated periodic model call during an evaluation. There is no reliable token or cost budget because Runtime usage is
not yet captured; do not treat these time limits as spending limits.

### Exit-code contract for `start`

`start` returns process exit code `0` unless at least one attempt in this
invocation ended in `state=failed` with `classification=infrastructure_failed`
(an unexpected local error such as a startup, API, or I/O failure). All other
terminal outcomes return `0` and are distinguishable only through `list` or
the stored evidence:

- `completed`: the workflow ran to a terminal event. A measured failing
  `code_correctness` (generated code that fails the acceptance suite) is still
  a successfully executed evaluation and exits `0`.
- `blocked` with classifications such as `provider_blocked`, `timed_out`,
  `configuration_blocked`, `submission_uncertain`, `acceptance_unavailable`,
  `evidence_corrupt`, or `interrupted`: the evaluation could not be completed,
  but the queue, evidence, or provider pipeline was not at fault in a way that
  invalidates the attempt; exits `0` so that investigation is not conflated
  with harness failure.
- `failed` / `infrastructure_failed`: nonzero exit (`1`).

The private queue directory contains:

```text
queue.sqlite3                 durable tasks and state transitions
tasks/<task-id>/              pinned fixture, prompt, suite, variant, template
attempts/<attempt-id>/        workspace, metadata, expanded team spec, evaluation
attempts/<attempt-id>/evaluation/runs/<id>/source/...
attempts/<attempt-id>/evaluation/runs/<id>/acceptance/...
attempts/<attempt-id>/evaluation/runs/<id>/result.json, report.md
runtime-state/<attempt-id>/    separate Studio state and private operational log
```

Each attempt launches a fresh production `agent-studio` process with a fresh
Runtime data root and unique Team ID. Its provider factory is process-global,
so a separate process prevents one variant from changing another. The queue
uses the existing workspace grant and project registration APIs, waits for the
P2 WebSocket readiness marker, then submits exactly one ordinary team message
to `POST /api/team/chat`. P2 collects and grades; P2.5 runs only with a
verified suite. Attempt metadata stores the source revision/dirty status,
binary digest, provider/model identities, template and effective team hashes,
and configuration fingerprint. The expanded team spec preserves effective
personas; credential fields are redacted. The API key lives only in the
private `runtime-state` directory during execution and is removed at normal
shutdown; that directory is separate from evidence packages.

On restart, the queue first looks for completed reports or a persisted terminal
chat message and grades recoverable evidence. An in-flight or uncertain HTTP
submission is never posted again. If no terminal can be proven, the attempt is
marked `interrupted` or `blocked` for investigation. Recovered reports note
missing live events rather than inventing them. A host crash can leave an
orphaned Runtime process or private `runtime-state/apikey`; inspect those before
manual cleanup. A newly built binary should be used for live work because the
queue records but cannot prove that a supplied binary corresponds to the source
revision. No real provider call is part of the automated tests.

## P4-A variant manifests

`variant.py` captures a reproducible identity for one Coding Team
implementation + configuration, and re-checks it later. It is a plain JSON
manifest plus two CLI verbs; there is no registry service.

```sh
python3 tools/eval/variant.py capture --variant-id baseline-p35 \
  --description "..." \
  --variant /path/to/variant.json \
  --template /path/to/coding-team-template \
  --binary /path/to/agent-studio \
  --fixture /path/to/pinned-fixture \
  --output /path/to/manifest.json
python3 tools/eval/variant.py verify /path/to/manifest.json
python3 tools/eval/variant.py verify /path/to/manifest.json \
  --attempt /path/to/queue/attempts/<id>
```

Each manifest records: source state (revision, dirty flag, full
`git status --porcelain --untracked-files=all` text with its SHA-256,
untracked file list, branch), Runtime binary SHA-256 with an explicit
source-provenance statement, template tree digest, per-member prompt SHA-256
digests, model tiers, credential-free `base_url`, scout interval, pinned
fixture/baseline digests, and a self `variant_digest` (SHA-256 over the
canonical manifest without `created_at`). `variant_digest` is the
configuration identity: the same variant name with different effective
content always produces a different digest, and `verify` refuses any
manifest whose digest does not recompute.

The three variant kinds:

- **Model variant**: a normal P3 `variant.json` with different tier models.
  `prepare --variant` already pins and fingerprints it; no new code needed.
- **Prompt variant**: point `prepare --template-dir` at a copied template
  with the modified prompt. The queue already pins the whole template tree;
  P4-A adds `template_prompt_digests` to attempt `metadata.json` so the
  effective per-member prompt identity is recoverable from each attempt.
- **Source variant**: build the modified Runtime in its own worktree and
  pass the resulting binary via `start --studio-bin`. The queue already
  records `runtime_binary_digest` per attempt; keep the binary immutable
  (or wrap it in a digest-checking shell wrapper, as the P3.5 evaluation
  did) because the queue cannot prove source-to-binary correspondence.

`verify --attempt` compares a manifest against attempt `metadata.json`
(binary digest, template and prompt digests, models, base URL, scout
interval, source revision and working-tree digest) and exits nonzero with a
problem list on any mismatch. A shared variant name is never treated as
proof of configuration identity. Historical P3 attempts lack
`template_prompt_digests`; verify reports that field as missing rather than
assuming equality.
