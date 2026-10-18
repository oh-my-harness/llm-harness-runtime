# P5 Execution Decision Document

- **Status:** DECISION DOCUMENT — **NOT APPROVED.** Every recommendation
  below is a proposal until explicitly approved by the user. Nothing here
  authorizes execution, modifies runtime/evaluator code, or implements a
  feedback mechanism. Approval items are consolidated in §10.
- **Inputs:** [P5 execution readiness](P5-execution-readiness.md),
  [P5 pre-registration](../preregistrations/2026-10-p5-failure-feedback-loop.md),
  [methodology](../methodology.md), [metrics](../metrics.md),
  P4-C ([report](../runs/2026-10-13-p4c-aa-baseline-v2.md)), P4-D-r1
  ([abort](../runs/2026-10-13-p4d-r1-abort.md)), P4-D-r2
  ([report](../runs/2026-10-14-p4d-r2-b2-prompt-ab-final.md)).
- **Labeling per [methodology §1.3](../methodology.md):** each decision
  section states *verified facts*, *options with evidence-based
  tradeoffs*, a *recommendation* (explicitly not approved), and whether
  *human approval is required*. All eight recommendations require human
  approval; none is self-executing.

---

## 1. Task Selection

**Question:** which task seeds the failure-feedback study?

**Verified facts:**

- b2 v2 under the original prompt: deterministic failure signature — 0/3
  in P4-C and 0/3 in P4-D-r2 condition A, always
  `test_case_duplicates_inside_one_dict_add` + `test_report_total_duplicates`
  (8/10). Under the clarified prompt: 3/3 pass (10/10).
- b3 v2: 3/3 pass (8/8) in P4-C. No known failure mode.
- b2's spec was adjudicated in-scope (additive everywhere); the graded
  gap is demonstrably closable when the right information is present.
- b2's history is extensively documented in this repo; provider-side
  exposure to that documentation is unknowable, though identical failure
  replication across six slots suggests no practical contamination so
  far.
- The pre-registration's task criteria: known failure modes, recovery
  meaningfully gradeable, not saturated in either direction.

**Options and tradeoffs:**

| Option | Tradeoffs |
| --- | --- |
| (a) b2, original prompt | Known deterministic failure (lowest risk of "no failures to recover"); cheapest identity re-freeze (digests reusable from r2). Risks: A3 — the failure is proven information-fixable (r2 condition B), so feedback may be indistinguishable from generic clarity; contamination/history bias is highest here |
| (b) b2, clarified prompt | Cleaner spec, but no known failure mode exists — b2 passed 3/3 under B; attempt-1 failures may not occur. Violates the pre-registration's "known failure modes" criterion |
| (c) b3 with induced failure | Uncontaminated, but requires designing and freezing a new failure mode (a task-variant change — new identity item, new validation), which itself needs a study to characterize. Highest preparation cost |
| (d) Another pool task | Cleanest generality, but failure mode undocumented; a characterization phase consumes billable slots before the main study and invites post-hoc task selection (the exact A3 bias the pre-registration warns against) |

**Recommendation (NOT APPROVED):** Option (a) — b2 with the original
prompt. It is the only candidate satisfying all three pre-registration
task criteria today without new design work. The A3 risk is real but is
addressed by the falsification criteria (pre-reg §10: signature
unchanged, displaced failures, task dependence) and by decision 6
(third arm). Mitigation for contamination: re-verify the b2 failure
signature with the first two C-condition attempt-1 slots before any T
slot launches; if the signature does not replicate, stop and amend.

**Human approval required:** YES (pre-reg §12 open question 1).

## 2. Failure Feedback Payload

**Question:** which artifacts enter the attempt-2 treatment prompt, in
what format?

**Verified facts:**

- All five candidate artifacts exist in the established artifact chain
  (readiness §2.1): failing test names (`acceptance/rN.json`), assertion
  output (`rN.log`), reviewer findings with verdict classes
  (`chat.sqlite3`), error messages (run records), previous diff
  (`diff.patch`).
- r2 slot-1 recovery verified that snapshot-derived files are
  self-sufficient, hash-pinnable inputs.
- Pre-registration §4.2 already fixes the constraints: evidence-derived
  only, no authored commentary, no report/adjudication content, every
  artifact hash-pinned.
- P4-C verified that reviewer findings *including scope classifications*
  (P2/out-of-scope) were the existing failure information in the system,
  and the scoping layer overrode them.

**Options and tradeoffs:**

| Payload subset | Tradeoffs |
| --- | --- |
| (a) Test names + assertion output only | Purest execution-feedback signal; directly tied to the defect oracle; smallest confound surface. Misses reviewer findings the system already produced |
| (b) (a) + previous diff | Adds the agent's own prior work, letting it see what it already tried; moderate length growth |
| (c) (a) + (b) + reviewer findings with verdict classes | Most complete information; but passes scope classifications into the recovery attempt, which may reinforce the P4-C out-of-scope behavior — or may surface the defect with maximal force. Direction unknown; this is exactly the P6 question |
| (d) (a) + (b) + reviewer findings as *raw text* without verdict classifications | Includes the detection content while withholding the scoping decision; a middle path that does not pre-judge P6 |

**Recommendation (NOT APPROVED):** Option (a) — failed test names +
assertion output only, at attempt-2 launch scope. Rationale: it is the
only payload whose information content is unambiguously
failure-derived and reviewer-independent; it minimizes the length
confound and avoids importing the unresolved scoping question (P6) into
P5's treatment arm. Diff and reviewer findings are natural extension
arms for a later amendment if the primary result is positive but below
ceiling. The exact template (header line, quoting format, source-record
digests embedded as comments) must be authored and byte-frozen in the
pre-execution amendment.

**Human approval required:** YES (pre-reg §12 open question 2).

## 3. Retry / Snapshot Protocol

**Question:** how does attempt 2 execute, and what is its snapshot
baseline?

**Verified facts:**

- The frozen configuration never ran a two-attempt flow
  (`max_attempts=1` in all P4 runs); the control's retry behavior is
  unverified.
- Snapshot chains r0/r1/r2 per attempt are the measurement substrate and
  were proven recoverable (r2 slot-1).
- Fresh queue root per slot, no silent retries, fail-closed on
  infrastructure failure are established supervision behavior.
- `max_duration=3600 s` < 2 × `attempt_timeout=1800 s`: the current
  parameter set cannot contain two full attempts (verified arithmetic
  conflict from the readiness review).

**Options and tradeoffs:**

| Option | Tradeoffs |
| --- | --- |
| Attempt 2 as a fresh slot in a new runtime instance, seeded with the feedback addendum, workspace from the same fixture baseline | Matches the pre-registration's independent-pair default; cleanest attribution (no in-process state carryover); each attempt is a standard, fully-evidenced slot; two billable slots per pair. Cost: doubles slot count vs shared-seed designs |
| Attempt 2 as in-place continuation of attempt-1's workspace | Halves slot cost; but requires supervisor/runtime semantics that have never been exercised (frozen `max_attempts=1`), creates state-carryover confounds, and complicates the recovery-success definition |
| Attempt-2 baseline = fresh fixture | Recovery diff measures the full attempted fix; consistent with all prior snapshot semantics |
| Attempt-2 baseline = attempt-1's final snapshot | Smaller diff, but "recovery from failed state" conflates the fix with the prior attempt's partial work; harder to compare against control |

**Recommendation (NOT APPROVED):** Attempt 2 = fresh slot, new runtime
instance, workspace from the same fixture baseline; independent pairs
per condition (the pre-registration §9 default); attempt-2 baseline
snapshots from the fixture, exactly like attempt 1. This keeps every
slot structurally identical to the validated P4-C/r2 slot shape — the
only new element is the prompt content, which is the independent
variable. No in-place continuation.

**Human approval required:** YES (pre-reg §12 open questions 3 and 5).

## 4. Timeout Policy

**Question:** do timeout parameters change for the two-attempt flow?

**Verified facts:**

- The frozen Baseline v2 binary contains a built-in 90 s Orchestrator
  first-decision timeout; it fired fatally in P4-D-r1 (one non-ACTION
  reply at ~55 s → `timed_out`, exit 10). P4-C never triggered it in 6
  slots; r2 never triggered it in 6 slots. Observed rate: 1 fatal event
  in 13 completed-or-attempted slots.
- The live working tree's `ORCHESTRATOR_DECISION_TIMEOUT = 180 s`
  (uncommitted) never executed in any pinned run.
- `attempt_timeout=1800 s`, `max_duration=3600 s` are the frozen outer
  budgets; the latter is arithmetically insufficient for two 1800 s
  attempts.
- Any parameter change is a controlled-variable change and must be a
  declared deviation in the pre-execution amendment, never a mid-run
  edit (pre-reg §12 Q4; methodology §3.1).

**Options and tradeoffs:**

| Option | Tradeoffs |
| --- | --- |
| (a) Keep 1800 s attempts; set `max_duration = 2 × 1800 s + supervisor overhead` (e.g. 4000 s); keep the 90 s decision timeout as-is | Minimal deviation: only the one arithmetic conflict is fixed; the 90 s timeout remains the same frozen value that 12/13 prior slots ran under — consistent with methodology §3.2 (declared deviations only where required). Risk retained: r1-style first-decision timeout can kill slots (~9% of slots so far: 1 in 13) |
| (b) Also raise the 90 s decision timeout | Reduces r1-style attrition risk, but is a second controlled-variable change (runtime behavior), confounding the A/B with a binary change; r1's causality was never established (the report explicitly does not attribute the timeout to model behavior) |
| (c) Reduce attempt timeouts to fit `max_duration=3600 s` | No binary change, but cuts per-attempt budget below the validated 1800 s envelope; risks truncating legitimate attempts |

**Recommendation (NOT APPROVED):** Option (a) — keep the 90 s decision
timeout and 1800 s attempt budget unchanged; raise only `max_duration`
to contain the two-attempt flow. Rationale: the 90 s timeout is part of
the frozen identity that produced all validated results to date; changing
it now would make P5 differ from P4-D-r2 in two ways at once. The r1
event is handled by the existing fail-closed decision table (stop,
classify, replace via new dated manifest — the r1→r2 precedent), and
per-attempt exposure to the timeout is identical between C and T
conditions, so it does not bias the comparison. Expected attrition from
this failure mode should be stated in the analysis.

**Human approval required:** YES (pre-reg §12 open question 4).

## 5. Sample Size

**Question:** how many recovery pairs per condition?

**Verified facts:**

- P4-D-r2 detected its effect at n=3/condition only because it was
  maximal (3/3 vs 0/3); its own report flags n=3 as exploratory only.
- The pre-registration proposes 5 recovery pairs per condition as a
  design proposal, finalized at pre-execution review.
- r2's identity pattern supports ≥5 repetitions with mechanical per-slot
  equality verification; slot cost is real (billable).

**Options and tradeoffs:**

| Option | Tradeoffs |
| --- | --- |
| (a) 5 pairs/condition (pre-reg proposal) | Matches the pre-registration; bounds coarse effect sizes; 20 slots total with independent pairs (10 attempt-1 + 10 attempt-2 under the §3 recommendation). Still exploratory — cannot confirm |
| (b) 3 pairs/condition (r2-sized) | Cheapest; but P5's expected effect is noisier than r2's maximal effect; high risk of an uninformative null |
| (c) ≥8–10 pairs/condition | Stronger bounds; materially higher cost; beyond what an exploratory study can justify before any effect estimate exists |

**Recommendation (NOT APPROVED):** Option (a) — 5 recovery pairs per
condition, consistent with the pre-registration proposal. Explicitly
exploratory; a confirmatory follow-up with larger n is the documented
path if H-FB survives. Slot order frozen and interleaved (C, T, C, T,
…) in the pre-execution amendment.

**Human approval required:** YES (pre-reg §9; final number is fixed
only in the pre-execution review).

## 6. Third Arm Decision

**Question:** pre-register the length-matched neutral-text arm now, or
conditionally after a positive primary result?

**Verified facts:**

- The treatment prompt is longer than control by construction; token
  budget equality is unverifiable (no telemetry).
- Alternative A1 (information volume) is pre-declared in the
  pre-registration; the falsification criteria include "improvement
  disappears when controlling for information amount."
- The length-matched arm is currently only a §4.3 proposal requiring an
  amendment; no such arm exists.

**Options and tradeoffs:**

| Option | Tradeoffs |
| --- | --- |
| (a) Pre-register the third arm now (3 arms × 5 pairs = 30 slots) | A1 confound answerable in the same study; a null on the length-matched arm vs treatment is direct evidence for feedback-specific value. Cost: +50% slots, +analysis complexity |
| (b) Two arms now; third arm only if the primary result is positive | Cheaper upfront; but a positive result without the control leaves the A1 confound open, and adding the arm later is a second authorization cycle |

**Recommendation (NOT APPROVED):** Option (b) — two arms now, with the
third arm pre-registered *conditionally*: if the primary result is
positive, a dated amendment adds the length-matched arm before any
confirmatory claim is made. Rationale: at exploratory status, the
cheapest design that can falsify H-FB is two arms; the A1 confound is a
*qualification* issue, not a falsification blocker, and §10 already
forces the confound to be reported with any positive result. This keeps
the first study's cost bounded while not closing the door on the
control.

**Human approval required:** YES (pre-reg §12 open question 6).

## 7. Telemetry Limitations

**Question:** run without token/cost telemetry, or pre-register a
telemetry change first?

**Verified facts:**

- `tokens`, `api_cost`, `human_attention`, `event_stream_integrity` were
  `not_measured` in every P4-C/P4-D slot (verified in both reports).
- The treatment prompt is larger than control by construction; budget
  equality is therefore unverifiable.
- A telemetry runtime change requires its own pre-registration and is
  out of scope for this experiment's one-changed-factor rule
  (methodology §3.2; roadmap cross-cutting requirements).
- Wall-clock duration is the only current efficiency signal (weak
  proxy).

**Options and tradeoffs:**

| Option | Tradeoffs |
| --- | --- |
| (a) Run P5 without telemetry; declare budget equality unverified | No runtime changes; the one-changed-factor discipline holds; limitation restated in analysis. Risk: any positive result carries an unquantified "more input tokens" confound (A1) |
| (b) Pre-register a token/cost telemetry runtime change first | Answers A1 partially; but adds a full pre-registration cycle, a binary change, and delays the study; it is a separate experiment-scale task per the roadmap's cross-cutting rules |

**Recommendation (NOT APPROVED):** Option (a) — run without telemetry,
with the declared unverified-budget assumption restated verbatim in the
analysis (pre-registration §6 already mandates this). Rationale: P5's
primary metrics (`code_correctness`, recovery success rate) do not
require budget verification; the confound is bounded by the payload
decision (§2 recommendation keeps the addendum minimal). Telemetry
remains the roadmap's separately pre-registered track, and becomes more
valuable before P7 (architecture comparison) than before P5.

**Human approval required:** YES (this choice affects the analysis
language and must be approved as a declared limitation).

## 8. Implementation Prerequisites

**Question:** what must be built (outside this documentation effort)
before launch, and in what order?

**Verified facts:**

- The feedback addendum builder does not exist; implementing it now is
  explicitly out of scope (standing constraint).
- The control's "normal retry instruction" text has never existed in
  this program (no prior retry); its bytes are an identity item.
- Two pinned evaluator files (`e2e_queue.py`, its test) exist only in
  the durable archive (`/home/liyufeng/p4c-aa/frozen-source/…`);
  verified at archival time, needs re-verification at freeze.
- The acceptance runner has no retry/backoff for transient namespace
  failures; r2 recovered slot 1 manually via preserved snapshots.

**Sequenced prerequisites (recommendation, NOT APPROVED):**

1. Author and byte-freeze (a) the control retry-instruction text and
   (b) the treatment feedback-addendum template (decision 2's payload
   subset) — pure authoring, no code.
2. Create the dated identity manifest (r2 pattern) covering all
   methodology §3.1 layers + the new prompt artifacts; re-verify the
   evaluator archive integrity at freeze time.
3. Pre-execution amendment to the pre-registration recording: task
   choice, payload bytes, retry protocol, timeout policy, sample size,
   slot order, seeding scheme, telemetry declaration, control
   instruction bytes.
4. *(Optional, separately pre-registered runtime change)*
   acceptance-runner retry/backoff for transient namespace failures —
   r2 lesson; reduces class-3 attrition but is not required (r2
   recovered without it). Do not bundle into P5's identity.
5. User authorization → execution.

**Human approval required:** YES — both for the sequencing itself and
for each implementation step as it becomes due (step 4 is explicitly a
separate gate).

---

## 9. Recommended experimental configuration (NOT APPROVED — summary of recommendations only)

| Parameter | Recommendation |
| --- | --- |
| Task | b2-inventory-merge v2, original prompt (contingent on signature replication in the first C attempt-1 slots) |
| Conditions | C (normal retry instruction, no structured feedback) vs T (frozen feedback addendum: failed test names + assertion output) |
| Architecture | coding-team v6 (orchestrator + coder + reviewer), unchanged |
| Model/provider | `p35-glm-5-3-flash-https` (GLM-5.3-Flash), re-pinned at freeze |
| Runtime | Frozen Baseline v2 binary/wrapper/CA incl. 90 s decision timeout (unchanged) |
| Evaluators | Grader p1.1, runner p2.5.0, frozen queue/collector/variant digests (archive re-verified) |
| Sandbox | bwrap 0.11.1 |
| Attempt structure | Independent pairs; attempt 2 = fresh slot, fixture baseline; no shared seeds (default) |
| Budget | `attempt_timeout=1800 s` per attempt; `max_duration` raised to contain two attempts (declared deviation); decision timeout unchanged |
| Sample size | 5 recovery pairs per condition; 20 slots; exploratory |
| Slot order | Frozen, interleaved C,T,C,T,… |
| Seeding | Attempt-2 feedback from same-study attempt-1 graded failures only; source digests frozen per T slot |
| Third arm | Not included in the first study; length-matched arm only via dated amendment if the primary result is positive |
| Telemetry | None added; budget equality declared unverified in pre-registration and analysis |
| Decision table | Fail-closed, no silent retries, outcome classes per methodology §4 |

## 10. Remaining approval items (all require explicit human sign-off)

1. **Task selection** — b2-original recommended; approval required,
   including the signature-replication stop rule (§1).
2. **Feedback payload** — test names + assertion output, byte-frozen
   template (§2).
3. **Control retry-instruction bytes** — must be authored and approved;
   never existed before (§3, readiness §6 item 15).
4. **Retry/snapshot protocol** — fresh-slot, fixture-baseline, independent
   pairs (§3).
5. **Timeout policy** — `max_duration` raised to fit two attempts; 90 s
   decision timeout unchanged; declared as deviations in the
   pre-execution amendment (§4).
6. **Sample size and slot order** — 5 pairs/condition, 20 slots,
   exploratory; finalized and frozen at pre-execution review (§5).
7. **Telemetry declaration** — run without token/cost capture, limitation
   restated in analysis (§7).
8. **Authorization to execute** — separate, explicit; billable slots;
   only after items 1–7 are approved and the pre-execution amendment is
   frozen.
9. **Implementation prerequisites** — the feedback-addendum builder and
   (optionally) acceptance-runner retry/backoff remain out of scope until
   separately approved and pre-registered (§8).

**Bottom line:** eight recommendations, zero approvals. The recommended
configuration is internally consistent and ready to be frozen into a
pre-execution amendment as soon as each item is explicitly approved.

## Files created

- `docs/e2e-evals/designs/P5-execution-decision.md` (this document)

## git status

- New untracked file inside the (still-untracked) `docs/e2e-evals/`
  directory tree: `designs/P5-execution-decision.md`.
- Nothing committed; runtime/evaluator code untouched; pre-existing
  working-tree state (modified docs, staged file, other untracked paths)
  unchanged.
