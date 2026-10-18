# P4-C A/A Baseline v2 — Pilot Report

- **Experiment ID:** `p4c-aa-baseline-v2`
- **Date:** 2026-10-13 (UTC; slots ran 03:13–03:57)
- **Type:** single-configuration A/A repeatability pilot. It does **not**
  compare different architectures or configurations; every slot used the
  identical frozen stack.
- **Evidence root:** `/home/liyufeng/p4c-aa/runs/pilot-20261013T111323`
  (run log: `/home/liyufeng/p4c-aa/runs/pilot-20261013T111323.log`)

## 1. Objective

Measure repeatability of the agentic coding team E2E pipeline on two pinned
benchmark tasks: run the same frozen configuration three times per task
(interleaved b2/b3), grade each attempt with the pinned offline grader, and
compare outcomes across attempts. Success criterion: every slot completes and
produces a valid graded outcome with a measured `code_correctness`.

## 2. Configuration identity (frozen)

All values below are recorded in the supervisor identity block
(`pilot-20261013T111323.log`, line 1; also `/home/liyufeng/p4b-benchmark/p4c-aa-identity.json`) and confirmed per attempt in each attempt's `metadata.json`.

| Item | Value |
| --- | --- |
| Configuration fingerprint | `e1138a5b923450b512651d73680024f41af12c32b2dfa4b14f2248015038f844` (identical for all 6 slots) |
| Variant | `p35-glm-5-3-flash-https` (`variant_sha256` `db90e75a…b6e1ba`) |
| Model | `GLM-5.3-Flash` for all team members (`strong`/`main`/`cheap` all map to it), provider `api.hyper-op.com` (OpenAI-compatible defaults; per-request overrides not exposed by Studio settings) |
| Team template | `coding-team` v6, effective members: orchestrator, coder, reviewer (template file digests in the supervisor identity block) |
| Runtime binary | wrapper digest `0cb74fb6…784216`; bundled binary `aaffd340…0610a`; CA bundle `69ff0cf2…bcef4` |
| Evaluator versions | grader `p1.1` (`offline_grader.py` `4325acb7…42762e680`), acceptance runner `p2.5.0` (`acceptance_runner.py` `6c7be50a…3732c3d8`), `e2e_queue.py` `111a494a…7264f0e`, `studio_collector.py` `44ec16a1…4db36acd7fb6f062c4c24`, `variant.py` `596ee3c3…e0e9057879af3d16b6` |
| Sandbox | `bwrap` (bubblewrap 0.11.1), Python 3.14, unshared namespaces per `tools/eval/README.md` P2.5 backend |
| Tasks | `b2-inventory-merge` v2 (def SHA-256 `099cd84e…f766`), `b3-rate-limiter` v2 (def SHA-256 `5136e06f…87689d`); task v2, suite v2, fixture v1 digests in each `metadata.json` |
| Supervisor parameters | `max_attempts: 1`, `attempt_timeout: 1800s`, `max_duration: 3600s` |

Per-slot configuration digests (identical across slots unless noted):
`configuration_fingerprint`, `suite_digest`, `prompt_digest` (b2:
`d014886d…44cb47c`; b3: `1f142bf7…369ca80`), `fixture_digest`,
`acceptance_spec_digest`. `effective_team_digest` differs per slot because it
binds the per-attempt team instance; prompt/template/task digests were equal.

## 3. Official outcomes (frozen; do not relabel)

Grader `p1.1`, metric `code_correctness` (measured, from independent unittest
assertions in the bwrap sandbox). All six attempts completed normally and the
Supervisor exited 0 (`pilot_complete` event: "all six slots produced valid,
decision-table-continue outcomes").

| Slot | Task | code_correctness | Acceptance counts (final round) | Evidence (`result.json`) |
| --- | --- | --- | --- | --- |
| 1 | b2-inventory-merge | **false** | 8/10 passed, 2 failed (r1, exit 1) | `attempt-1-b2-inventory-merge/attempts/df8cd14b-dcbe-479f-b80b-1f72e5034f7d/evaluation/runs/8dd394f4f212f67274ff20f7/result.json` |
| 2 | b3-rate-limiter | **true** | 8/8 passed (r1, exit 0) | `attempt-2-b3-rate-limiter/attempts/12da790b-74aa-40db-ad1c-9cf94d3ec3cf/evaluation/runs/b4d14b3c9f7892e936cd556c/result.json` |
| 3 | b2-inventory-merge | **false** | 8/10 passed, 2 failed (r1, exit 1) | `attempt-3-b2-inventory-merge/attempts/fe383383-7312-4d56-af48-cb7d86ea7abd/evaluation/runs/efe3ae831713ed41cf6da83f/result.json` |
| 4 | b3-rate-limiter | **true** | 8/8 passed (r1, exit 0) | `attempt-4-b3-rate-limiter/attempts/427d7f1b-2850-4e79-b0bb-07512f04a1ca/evaluation/runs/3dfc41487c9691b8e9b782a8/result.json` |
| 5 | b2-inventory-merge | **false** | 8/10 passed, 2 failed (r1 and r2 identical counts, exit 1) | `attempt-5-b2-inventory-merge/attempts/d235298e-9e70-4d15-a687-7fee9f0781d5/evaluation/runs/b5fcec961295ed0753518688/result.json` |
| 6 | b3-rate-limiter | **true** | 8/8 passed (r1, exit 0) | `attempt-6-b3-rate-limiter/attempts/9021b57a-d278-4790-8684-e5d4e2903271/evaluation/runs/c85d048b98f3414ea02bfc39/result.json` |

Aggregate: b2 failed 3/3 (8/10 each), b3 passed 3/3 (8/8 each). All paths are
relative to `/home/liyufeng/p4c-aa/runs/pilot-20261013T111323/`.

Failing b2 tests (identical in every b2 attempt, per `acceptance/rN.log`):
`test_case_duplicates_inside_one_dict_add`, `test_report_total_duplicates`.

## 4. Execution profile

Workflow wall time from `pipeline_started` to `pipeline_completed` in each
attempt's `evidence/events.jsonl` ( Supervisor `runtime-state/.../logs/runtime.jsonl` also present):

| Slot | Task | Duration | Orchestrator dispatches | Revision rounds |
| --- | --- | --- | --- | --- |
| 1 | b2 | 304 s | 2 | 0 |
| 2 | b3 | 662 s | 2 | 0 |
| 3 | b2 | 303 s | 2 | 0 |
| 4 | b3 | 596 s | 2 | 0 |
| 5 | b2 | 433 s | 4 | 1 (review round 1 → r2) |
| 6 | b3 | 305 s | 2 | 0 |

Metrics recorded `not_measured` by the grader (all six attempts, reasons in
`result.json`): `api_cost` ("workflow cost unavailable"), `tokens` ("workflow
usage unavailable"), `event_stream_integrity` ("valid start and terminal rows do
not prove no intermediate events were lost"), `human_attention` ("interventions
not fully captured"). `revision_improvement` was measurable only in slot 5
(r2 present): value 0 — "change in passed tests" 0; the other slots have no r2,
so revision improvement is `not_measured` there (the grader does not treat a
missing r2 as a failed revision).

## 5. Analysis and review trail

### 5.1 Verified observations

- All six slots used the identical configuration fingerprint; no configuration
  drift between attempts.
- b2 failed with the same two acceptance tests in all three attempts
  (`test_case_duplicates_inside_one_dict_add`, `test_report_total_duplicates`).
- The b2 prompt (identical digest in slots 1/3/5, `prompt.txt`) says: "when an
  incoming SKU already exists in current stock (compare SKU codes
  case-insensitively), its quantity must ADD … Returned keys must stay
  lowercase." The failing tests exercise case-colliding duplicate keys
  *within `current` itself*.
- All three b2 Reviewers identified the residual defect and classified it P2
  (non-blocking, out of scope). Slot 1 review: "case-variant duplicate SKUs
  within the `current` dict itself … are last-write-wins rather than summed …
  no action taken". Slot 3 review: same P2-1 finding, "pre-existing baseline
  behavior, unchanged by this fix and outside the stated objective". Slot 5
  round-1 review: "Two P2 findings below" (stale docstring + the
  `current`-internal collision).
- In slot 5 the Orchestrator's round-2 revision assignment
  (`chat.sqlite3` message id 10, "REVISION ASSIGNMENT (round 2)") explicitly
  declared the `current`-internal collision "Explicitly OUT OF SCOPE … Leave
  that loop untouched; behavior stays as-is" and ordered a docstring-only
  revision. The coder complied; r2's merge body was byte-identical to r1, and
  the grader recorded `revision_improvement = 0`. The docstring-only revision
  did not change acceptance results (8/10 in both r1 and r2).
- Slot 5's terminal package and coder report explicitly confirm the
  out-of-scope P2 was "left untouched as instructed".

### 5.2 OMP's initial analysis (superseded in part)

Initial read attributed the b2 failures to a shared defect: the suite's
`test_case_duplicates_inside_one_dict_add` exercises case-colliding keys
*within* `current`, while the operator prompt only specifies incoming-vs-current
addition — a prompt/suite mismatch, i.e. the task prompt under-specifies the
graded behavior, so the benchmark item could be considered invalid.

### 5.3 Codex's independent review (correction)

Codex reviewed the task's `README.md` and `inventory.py` docstring in the
pinned baseline and found they state a **broader additive-merge contract**:
"All keys are SKU codes; matching is case-insensitive; merged keys are
lowercase; quantities add" (workspace `README.md`), and the `merge_inventories`
docstring says "Quantities for the same SKU add up" — not limited to the
incoming-vs-current direction. Under that reading, the `current`-internal
case-collision behavior is in-scope, and the benchmark cannot simply be
declared invalid on prompt/suite-mismatch grounds. The spec dispute is real and
unadjudicated; the recorded `code_correctness` results stand as graded either
way.

### 5.4 Key coordination finding

In all three b2 attempts, every Reviewer independently identified the
remaining case-collision issue but treated it as out of scope and non-blocking.
In slot 5 the Orchestrator went further: the round-2 assignment explicitly
prohibited fixing it ("Explicitly OUT OF SCOPE … Leave that loop untouched")
and scoped the revision to a docstring-only change. The docstring-only revision
did not improve acceptance (r1 and r2 both 8/10). No attempt ever attempted the
collision fix. This is the coordination finding: the pipeline's scoping
(decision) layer converged on "in-scope = incoming-vs-current only", so the
graded gap was never actioned despite three independent detections.

### 5.5 Disputed interpretations

- Whether b2's graded behavior is fully implied by the task materials (README
  + docstring say "quantities add" broadly; the operator prompt scopes the fix
  to incoming-vs-current). This is a specification-scope question, not a
  runtime defect; it needs adjudication, not retroactive relabeling.
- Whether the Reviewers' "out of scope" classification is correct follows from
  the same dispute; under the broader reading it was a mis-scoping.

### 5.6 Hypotheses (not verified)

- A clarified prompt that explicitly covers duplicates within `current` would
  likely let the coder close the two failing tests; untested.
- The slot-5 docstring-only revision cycle demonstrates that revision rounds
  can be consumed on non-behavioral findings when review verdicts contain no
  P0/P1 items; whether that generalizes is untested (n = 1 revision event).

### 5.7 Decisions not yet made

- Independent adjudication of the b2 specification (see below) — not done.
- Whether to run a controlled prompt A/B (P4-D). **Not authorized and not
  started**; nothing in this report should be read as approval or completion.
- Whether to extend metrics collection (cost/tokens/human-attention capture)
  and add a read-only E2E monitoring interface — proposals only, no decision.

## 6. Evidence index

All paths relative to the evidence root
`/home/liyufeng/p4c-aa/runs/pilot-20261013T111323/` unless absolute.

| Artifact | Path |
| --- | --- |
| Supervisor log (all slots) | `supervisor-log.jsonl`; run log `/home/liyufeng/p4c-aa/runs/pilot-20261013T111323.log` |
| Grader results | `attempt-N-*/attempts/<uuid>/evaluation/runs/<run>/result.json` (N = 1..6; exact paths in §3) |
| Acceptance records | same evaluation run dir: `acceptance/spec.json`, `fixture.txt`, `test_acceptance.py`, `r1.json`/`r1.log` (+ `r2.*` in slot 5) |
| Grader human report | same evaluation run dir: `report.md` |
| Snapshot manifests & files | `attempt-N-*/attempts/<uuid>/workspace/.pipeline/snapshots/<run-id>/r0..rN/` (`snapshot.json`, `files/`, `diff.patch`, `task-relative.patch`) |
| Workflow event streams | `attempt-N-*/attempts/<uuid>/evaluation/runs/<run>/source/evidence/events.jsonl` and `human-review-package.md` |
| Workflow chat records | `attempt-N-*/runtime-state/<attempt-id>/teams/<team-id>/chat.sqlite3` (slot 5 round-2 assignment: message id 10) |
| Pinned task definitions | `attempt-N-*/tasks/<task-id>/definition.json` (+ `prompt.txt`, `fixture/`, `acceptance/spec.json`) |
| Attempt configs | `attempt-N-*/attempts/<uuid>/metadata.json`, `effective-team.json`, `prompt.txt` |
| Supervisor identity | `/home/liyufeng/p4b-benchmark/p4c-aa-identity.json` (referenced by log line 1) |

Grader result SHA-256s are recorded inside each `result.json`'s own evidence
blocks; no raw evidence has been copied into this repository.

## 7. Open questions and next steps

1. **Independent b2 specification adjudication.** Decide, from the frozen task
   materials alone, whether the graded behavior (case-colliding duplicates
   within `current`) is in scope. Owner and method not yet chosen.
2. **Possible controlled prompt A/B (proposed P4-D).** Original vs clarified
   b2 prompt, everything else frozen. Proposed only — not authorized, not
   scheduled, not run.
3. **Metrics collection gaps.** `api_cost`, `tokens`, `human_attention`, and
   `event_stream_integrity` were `not_measured` in all six slots because
   workflow usage/cost and interventions are not captured by the current
   Studio-based flow. Any fix must not alter the frozen evaluator digests
   mid-experiment.
4. **Read-only E2E monitoring interface.** Desired: observe running slots
   without touching runtime state. Not designed, not approved, not built.

## 8. Preservation note: `/tmp/p4c-aa-ready`

The isolated pinned evaluator source tree at `/tmp/p4c-aa-ready` (796 KiB,
`MANIFEST.sha256` with 13 entries) contains the exact evaluator sources pinned
for this pilot. Status as of 2026-10-13:

- `tools/eval/{acceptance_runner,e2e_queue,offline_grader,studio_collector,variant}.py`
  in `/tmp/p4c-aa-ready` match the pilot's pinned `evaluator_sha256` values
  byte-for-byte, and are byte-identical to the live
  `/home/liyufeng/Projects/llm-harness-runtime/tools/eval/` copies — **except**
  `e2e_queue.py` and `test_e2e_queue.py`, where the live repo has moved ahead
  (repo now enforces `execution_mode == "evaluation"` at startup and has
  corresponding test changes; the pinned copy predates that).
- `preservation/e2e_queue.v0-policy.py` and
  `preservation/test_e2e_queue.v0policy.py` equal the *live repo* files;
  `preservation/test_e2e_queue.pinned-era.py` matches the *pilot-era*
  `test_e2e_queue.py` (differs from live by the same `execution_mode` changes).
- Therefore: **`/tmp/p4c-aa-ready` still needs durable preservation.** Two
  pinned files (`tools/eval/e2e_queue.py`, `tools/eval/test_e2e_queue.py` —
  digests `111a494a…` and `a5a2b909…`) no longer exist anywhere in the runtime
  checkout, and the `/tmp` location is volatile. The repo's current
  `tools/eval/e2e_queue.py` (digest `8e2b9a38…`) is the post-pilot evolution
  and does not satisfy the pinned-evaluator hash. A future move into a
  permanent archive location is recommended; no action was taken here
  (documentation-only change constraint).

### Archived copy (2026-10-13)

The isolated pinned evaluator source was preserved byte-identically at
`/home/liyufeng/p4c-aa/frozen-source/p4c-aa-ready-20261013` (copied with
`cp -a`; original `/tmp/p4c-aa-ready` untouched). Verification of the archive:
all 32 files match the source SHA-256-for-SHA-256 (`diff -r` clean), the
archived `MANIFEST.sha256` self-check passes 13/13 (`sha256sum -c`), and the
pinned `tools/eval/e2e_queue.py` equals
`111a494ae2d4b8f71a3aecaf11406fa7df8d17e5334505d35204c3d867264f0e`. The three
era variants remain distinguishable in the archive: pilot-era
`test_e2e_queue.py` = `a5a2b909…` (both `tools/eval/` and
`preservation/test_e2e_queue.pinned-era.py` copies), post-pilot/live-era
`test_e2e_queue.v0policy.py` = `e2f3ba42…` and `e2e_queue.v0-policy.py` =
`8e2b9a38…` (equal to the current runtime checkout). Pre-copy inspection found
no embedded credentials (token references are runtime parameters or the
literal test fixture `test-token`) and no symlinks.
