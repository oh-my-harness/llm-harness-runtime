# P5 Protocol Amendment — Recovery-Pair Semantics (Pre-Implementation)

- **Date:** 2026-10-14 (Asia/Shanghai).
- **Scope:** Agentic Coding Team E2E Evaluation in `llm-harness-runtime`.
- **Status:** Protocol decisions frozen by this amendment. P5 implementation
  has **NOT started**; no P5 experiment has executed. Documentation only:
  no runtime/evaluator changes, feedback implementation, or experiment execution.
- **Amends:** [P5 preregistration](2026-10-p5-failure-feedback-loop.md) and
  [P5 execution amendment](2026-10-p5-failure-feedback-loop-execution-amendment.md).
  This document governs the protocol details and corrections below. Task,
  architecture, budgets, evaluator, and standing stop rules otherwise remain
  as recorded there. This document does not authorize implementation or execution.
- **Labels:** **VERIFIED** attributes findings to the completed read-only audit
  and existing records; **DECISION** is a normative protocol choice;
  **FUTURE** identifies work still to be done. Decisions are not measured results.

## 1. Pair model [DECISION]

P5 is a **recovery-pair experiment**, not a slot comparison. The experimental
unit is an attempt-1 failure and its own attempt-2 recovery opportunity.
Conditions compare recovery rates over these pairs. Runtime slots are execution
and evidence containers; they are not independent recovery samples.

| Term | Frozen definition |
| --- | --- |
| `experiment` | The study `p5-failure-feedback-loop`, qualified by its dated identity-manifest revision. Historical P4 studies and later identity revisions are not pooled with it. |
| `pair_id` | Immutable pair key, unique within the experiment: `p5-pair-C-01` through `p5-pair-C-05`, and corresponding `T` keys. Assigned before attempt 1; never reused for a replacement. |
| `attempt_id` | Immutable execution key: `<pair_id>-a1` or `<pair_id>-a2`. Exactly one execution per key. Slot names and runtime IDs map to this key in the manifest. |
| `parent_attempt_id` | `null` for attempt 1; exactly the same pair's `a1` ID for attempt 2 in both conditions. No cross-pair parent or shared seed is permitted. |
| `attempt_1` | Fresh runtime and queue root, fixture-baseline workspace, standard task prompt; run and independently grade before deciding whether to launch attempt 2. |
| `attempt_2` | One fresh runtime and queue root on the same fixture baseline, with the standard prompt plus the condition's frozen addendum. Launch only after a valid attempt-1 failure. Never resume attempt 1. |
| `condition` | Immutable `C` (neutral retry instruction, no failure evidence) or `T` (failed test names and assertion output from its own parent). Assigned before attempt 1 and recorded on both attempts. The only treatment difference is attempt-2 addendum content. |

Attempt-1 inputs and settings are identical across conditions. One attempt 1
has at most one attempt 2; no third attempt, shared failure seed, or silent
replacement. A planned attempt-2 ID can exist without an execution; its launch
state must be recorded explicitly.

The existing plan reserves five pairs per condition: ten attempt-1 slots and
at most ten conditional attempt-2 slots. **Twenty slots is a ceiling**, not a
promise of twenty executions or ten evaluable recovery pairs. Attempt-1 passes
and attrition reduce the recovery sample without replacement.

### 1.1 Canonical mapping correction

The execution amendment repeats `C-a1-p3` at slot 11 and labels slot 15
`C-a1-p4`, although their children are `C-a2-p4` and `C-a2-p5`. This amendment
corrects those labels. The planned slot order, conditions, and parent-slot
links are unchanged; the manifest must use this unambiguous mapping:

| `pair_id` | Attempt-1 planned slot | Attempt-2 planned slot | Parent slot |
| --- | --- | --- | --- |
| `p5-pair-C-01` | 1 | 3 | 1 |
| `p5-pair-C-02` | 2 | 4 | 2 |
| `p5-pair-T-01` | 5 | 6 | 5 |
| `p5-pair-C-03` | 7 | 8 | 7 |
| `p5-pair-T-02` | 9 | 10 | 9 |
| `p5-pair-C-04` | 11 | 12 | 11 |
| `p5-pair-T-03` | 13 | 14 | 13 |
| `p5-pair-C-05` | 15 | 16 | 15 |
| `p5-pair-T-04` | 17 | 18 | 17 |
| `p5-pair-T-05` | 19 | 20 | 19 |

## 2. Recovery metric [DECISION]

Per-attempt classes retain [methodology §4](../methodology.md): class 1 is a
valid functional measurement, class 2 an execution-reliability failure, and
class 3 an infrastructure/provider failure. A timeout or missing measurement
is never converted to `code_correctness=false`.

**Valid attempt-1 failure** is an eligibility predicate, not a terminal pair
outcome: attempt 1 completed validly, its evidence and identity verify, and the
frozen grader measured `code_correctness=false` (class 1). Only this predicate
permits attempt 2. A valid pair additionally requires an admissible attempt 2
with a class-1 measurement under the feedback and identity contracts.

| Terminal pair outcome | Definition | Recovery-rate membership |
| --- | --- | --- |
| `recovery_success` | Valid attempt-1 failure; admissible attempt 2 has class-1 `code_correctness=true`. | Numerator and denominator |
| `recovery_failure` | Valid attempt-1 failure; admissible attempt 2 has class-1 `code_correctness=false`. | Denominator only |
| `infrastructure_failure` | Established class-3 failure at either attempt or its grading stage. The pair is ineligible for functional recovery analysis. | Excluded; report stage, cause, and evidence separately |
| `invalid_pair` | No admissible pair measurement for another reason: class-2 failure, unresolved causality, missing evidence, hash/parent mismatch, prohibited feedback, protocol drift, or eligible attempt 2 left unexecuted when the study closes. | Excluded; report explicit reason and any measured attempt-1 result |
| `no_recovery_opportunity` | Valid attempt-1 class-1 pass; attempt 2 is not launched. | Excluded; report count |

Before closure, an eligible pair awaiting attempt 2 is `pending`, not a
recovery failure. A pair whose attempt 1 never launched is `not_started`;
report it separately from observed outcomes. Terminal categories are disjoint.
If several problems coexist, record all reasons: established infrastructure
failure determines `infrastructure_failure`; otherwise a contract/identity or
reliability defect determines `invalid_pair` before functional outcomes are
considered. Evidence with unverifiable provenance cannot establish success.

For each condition separately:

```
p5_functional_recovery_rate = recovery_success /
    (recovery_success + recovery_failure)
```

A zero denominator is `not_measured` with reason `no_valid_recovery_pairs`,
not zero percent. Report counts of allocated pairs, launched attempts, valid
attempt-1 failures, every terminal outcome, pending/not-started pairs, and the
actual functional denominator by condition. This exposes attrition; the rate
is conditional on both attempts being functionally evaluable. A C–T rate
difference is exploratory and requires both denominators to be nonzero.

Success means the frozen binary correctness criterion passes, not merely
fewer failed tests. Per-test improvements are secondary evidence. Functional
correctness, execution reliability, and infrastructure outcomes remain separate.

**Metric naming:** P5 functional recovery is distinct from the measurement-layer
snapshot recovery called “Recovery success rate” in [metrics §2](../metrics.md).
Regrading preserved evidence is not attempt 2 and never counts as P5 recovery.
Original infrastructure records remain immutable; any later regrading requires
separate provenance and a dated amendment before changing analysis eligibility.

## 3. Feedback contract [DECISION]

The following allow-list is exhaustive for treatment feedback:

| Allowed content | Source and restriction |
| --- | --- |
| Failed test names | Verbatim names of failed, disclosed acceptance tests in the paired attempt-1 acceptance record. |
| Assertion output | Verbatim assertion-result text for those same failed tests in the paired attempt-1 acceptance log. |

Use the acceptance round/snapshot referenced by the authoritative attempt-1
correctness result, not an earlier round selected because it has a more useful
failure. Bind `acceptance/rN.json` and `acceptance/rN.log` to that result and
snapshot in the manifest. Failure names and assertions must agree between
records; absence or ambiguity invalidates the pair and stops continuation.

A frozen template may add neutral headers, delimiters, and test/output labels.
It may not add semantic content. Source digests and provenance belong in the
manifest, not in the agent-visible feedback. Preserve each selected name and
assertion excerpt byte-for-byte; order tests lexicographically by their exact
names. The extraction-format rules and template bytes must be pinned before
the first attempt 1; no per-pair editorial selection or truncation is allowed.
A `FAIL:` block is a source locator, **not permission to copy the whole block**.

Explicitly prohibited, even if present in a source log:

- **Grader internals:** evaluator/test source, scoring logic, thresholds,
  internal stack frames or source lines, grading commentary, and reports.
- **Hidden tests:** names, output, source, or hints from undisclosed tests.
- **Reviewer verdicts:** findings, scope judgments, P0/P1/P2 labels, and
  blocking/non-blocking decisions from attempt 1 or external reviewers.
- **Repair suggestions:** diagnosis, fixes, expected solution/code, authored
  advice, paraphrases, summaries, or editorial commentary from humans or models.
- Prior diffs, code/workspace files, conversations, runtime state, earlier-round
  failures, infrastructure logs, or artifacts from other pairs/experiments.

If assertion text itself contains prohibited content, do not redact or rewrite
it ad hoc: stop and record `invalid_pair` with the contract reason. A broader
feedback contract requires a new dated amendment. The C addendum contains only
frozen neutral retry text, with no parent test names or assertion output.

## 4. Attempt lifecycle [DECISION]

### 4.1 Attempt 1: run → grade → extract feedback

1. Verify the frozen attempt-1 identity; start a fresh runtime, queue root,
   and fixture-baseline workspace; run the standard agent workflow.
2. Preserve its snapshots, events, result, and supervision evidence; grade
   with the frozen evaluator; record the per-attempt outcome class.
3. If the valid-failure predicate holds, T extracts only the §3 feedback
   from that graded result. C records the same parent linkage but injects
   no failure evidence. Freeze feedback provenance and the composed attempt-2
   prompt/spec identity before proceeding.
4. A functional pass closes the pair without attempt 2. Class-2/class-3,
   ambiguous evidence, or contract failure stops under the standing decision
   table; never manufacture feedback from an infrastructure error.

### 4.2 Attempt 2: fresh runtime → inject feedback → run → grade

1. Verify its parent, condition, manifest chain, prompt, feedback, and all
   common identities before launch. Initialize a new runtime, new queue root,
   and workspace from the same original fixture baseline.
2. Inject the verified T addendum or C neutral retry instruction into the
   standard prompt **before the agent workflow runs**.
3. Run with the same model/provider, template, evaluator, and frozen runtime
   settings; preserve an independent evidence chain and grade independently.
4. Record attempt-2 class and terminal pair outcome under §2. No third attempt.

**Snapshots preserve evidence, not runtime state.** They support audit and
separately documented regrading. Attempt 2 must not restore attempt-1 snapshots,
final code, conversation, queues, memory, or runtime checkpoints. Its snapshot
chain starts at its own fixture baseline. Only the allowed T feedback crosses
from the parent into the agent input; C receives its predefined retry text.

The earlier signature-replication gate remains: the first two C attempt-1
slots must reproduce the registered failure signature before any T launch.
Infrastructure/evidence failures stop fail-closed; no silent retries or slot
replacement. `max_attempts=1` remains per queue-root execution: two separately
orchestrated attempts do not enable an internal queue retry mechanism.

## 5. Identity requirements [DECISION]

All required digests are full SHA-256 values over exact artifact bytes.
Abbreviated historical pins in earlier prose are references, not valid manifest
values. Structured descriptors use a byte-frozen canonical serialization;
file/tree manifests list sorted relative paths and individual file hashes.
Version labels alone do not substitute for artifact hashes.

| Required hash | Binding |
| --- | --- |
| `task_hash` | Task definition, baseline fixture/tree, original acceptance spec, and disclosed acceptance suite via a hash-pinned task descriptor. Same task and suite in both attempts/conditions. |
| `prompt_hash` | Exact rendered agent-input bytes for each attempt; also pin standard prompt, C retry text, and T template separately. T composed prompts are pair-specific. |
| `feedback_artifact_hash` | Exact injected T feedback bytes plus separately hashed source JSON/log records and excerpt provenance, bound to parent/result/snapshot. C records feedback as absent (`null` with reason `control_no_feedback`) and pins its retry addendum hash separately. |
| `evaluator_hash` | Descriptor of pinned grader, acceptance runner, queue/collector, and grading configuration. Use the frozen archive specified in the execution amendment; verify at grading time too. |
| `runtime_hash` | Descriptor of binary, wrapper, CA bundle, template, model/provider variant, sandbox/environment, and frozen operational parameters; individual component hashes retained. |
| `condition_hash` | Descriptor binding `C`/`T`, addendum policy/template hash, feedback contract version, and common configuration hashes. C and T descriptors differ by declared policy; a separate common-configuration fingerprint checks equality of controlled settings. |
| `pair_manifest_hash` | Hash of a canonical, immutable manifest revision binding experiment revision, pair/condition, both planned attempt IDs, parent relation, slot mapping, launch states, input hashes, evidence/result references, and previous-revision hash. |

Per-attempt acceptance-spec copies must pin the composed prompt digest in
`fixture_digest`, as required by the existing preparation contract. They are
individually hashed; only that prompt-binding field may differ from the
original spec. The acceptance suite and semantic grading criteria stay equal.
These derived spec bytes are declared identity consequences of the prompt
factor, not permission for evaluator changes.

The study identity manifest (`p5-fb-identity.json`) and pair revisions form an
append-only chain. Freeze IDs, common identities, C text, and T extraction
policy/template before attempt 1. After grading attempt 1, append its evidence,
feedback, prompt/spec hashes and parent binding **before attempt 2 launches**.
Append attempt-2 results after grading; never edit previous revisions.
Manifest hashes exclude their own hash field and are stored in the next
revision or a sidecar, avoiding self-reference.

Inputs must verify against a pre-launch revision; outputs cannot be pinned
before they exist and must instead be sealed after production. A post-launch
output seal cannot retroactively validate an unpinned input. Any identity drift,
chain break, wrong parent, or feedback mismatch stops the study. This amendment
creates no manifests, hashes of future artifacts, or verification mechanism.

## 6. Status separation

### VERIFIED — completed audit and prior evidence

- The supplied read-only audit found existing runtime primitives sufficient;
  no new runtime primitive is required for the planned pair lifecycle.
- The audit found no P5 two-attempt recovery experiment. The supplied current
  state says P5 implementation has not started; this amendment reports no
  control behavior, recovery rate, or P5 result.
- [Readiness §§2–5](../designs/P5-execution-readiness.md) and
  [checklist §§3,8](../designs/P5-final-approval-checklist.md) record existing
  acceptance artifacts, fresh-slot/snapshot patterns, and prior identity
  verification. These are inherited audit findings, not fresh archive/hash
  verification performed by this documentation task.
- P4-C and P4-D-r2 are completed, as recorded in their
  [P4-C report](../runs/2026-10-13-p4c-aa-baseline-v2.md) and
  [P4-D-r2 report](../runs/2026-10-14-p4d-r2-b2-prompt-ab-final.md).
  They supply methodological precedent, not P5 recovery-pair samples.

### DECISIONS — frozen here

Pair identity and corrected slot mapping (§1); eligibility, disjoint outcomes,
functional denominator and metric naming (§2); strict feedback allow-list and
prohibitions (§3); fresh-runtime lifecycle and evidence-only snapshots (§4);
seven required hash families and staged manifest binding (§5). These decisions
resolve protocol ambiguity without asserting an implemented capability.

### FUTURE — implementation and execution remain undone

1. Author and pin neutral C retry text, T wrapper, and deterministic extraction
   format consistent with §3.
2. Implement external pair orchestration, feedback extraction/injection, and
   manifest verification using existing runtime primitives; validate parent
   mapping, contract boundaries, and functional/infra separation.
3. Create identity/pair manifests with full actual digests, re-verify durable
   archives, and validate derived prompt/spec identities.
4. Perform the separately authorized preflight, including a no-model-call plan
   check. No preflight or dry run is executed by this amendment.
5. Obtain the standing explicit execution authorization, then run and analyze
   the planned pairs. Token/cost equality remains the earlier declared
   unverified assumption; no telemetry is implemented here.

## 7. Supersession and scope boundary

| Earlier wording | Resolution in this amendment |
| --- | --- |
| Fraction of attempt-1 failures converted to pass | Functional denominator includes only valid pairs with measured class-1 attempt 2; exclusions and eligible-failure counts reported (§2). |
| Framework “Recovery success rate” | Keep measurement-layer snapshot recovery distinct from `p5_functional_recovery_rate` (§2). |
| Twenty slots / duplicated C pair labels | Twenty planned slots maximum; conditional launches and corrected canonical mapping (§1). |
| Candidate reviewer findings, errors, prior diff, or whole `FAIL:` blocks | Only disclosed failed names and assertion-result excerpts; prohibitions apply within logs too (§3). |
| Condition fingerprint excluding prompts | Common fingerprint verifies equality; condition hash explicitly binds the policy and template (§5). |
| Every dependent hash frozen before launch | Inputs frozen before use; newly produced outputs sealed afterwards in chained revisions (§5). |

Original reports and earlier documents remain unchanged. Later protocol
changes require a dated amendment; no pooling, silent retries, infrastructure
imputation, or retrospective relabeling. This document completes the requested
protocol amendment only; runtime changes, implementation, and experiments
remain outside this task.
