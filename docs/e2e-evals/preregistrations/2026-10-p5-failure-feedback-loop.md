# P5 Pre-Registration — Failure Feedback Loop

- **STATUS:** **NOT EXECUTED. NOT AUTHORIZED. PRE-REGISTRATION ONLY.**
  This document defines the experiment before any execution. It does not
  authorize billable slots, does not modify runtime/evaluator code, and
  does not implement any feedback mechanism. Execution additionally
  requires explicit user authorization per
  [methodology §2](../methodology.md) (dated pre-registration → user
  authorization → frozen identity file → supervised execution).
- **Design document:** [P5-failure-feedback-loop.md](../designs/P5-failure-feedback-loop.md)
  (this pre-registration freezes the parts of it needed for execution;
  conflicts resolve in favor of this document).
- **Claim labels:** verified facts, hypotheses, and proposals are labeled
  per [methodology §1.3](../methodology.md). Nothing below claims the
  experiment has run or that feedback works.

---

## 1. Research Question

Does structured failure feedback improve agent recovery success compared
with retrying without structured failure information?

Why this matters for Agentic Coding Team: the goal of the system is not
only first-attempt generation but the ability to **use execution feedback
to recover from failure**. A team that passes only on a clean first
attempt and cannot metabolize its own failure evidence is a different
system, on the reliability axis, from one that can. Measuring this
separates "the agent lacks the information" from "the agent has the
information and still cannot act on it" — two bottlenecks that require
different fixes (more information vs capability/scoping changes).

This question is open. No prior experiment in this program has tested
feedback-conditioned recovery; nothing in P4-C or P4-D constitutes
evidence that feedback works, and this pre-registration claims no such
result.

## 2. Background and Motivation

Only verified lessons from completed experiments are used; each is cited
to its report section.

**From P4-C** ([report](../runs/2026-10-13-p4c-aa-baseline-v2.md)):

- *Verified (§5.4):* in all three b2 slots, Reviewers independently
  detected the residual case-collision defect but classified it P2
  (non-blocking, out of scope); the workflow did not recover — the one
  revision round was explicitly scoped to exclude the defect and produced
  `revision_improvement = 0`. No attempt ever fixed the graded gap.
- *Relevance:* failure was detected but not acted upon. Understanding how
  the team handles failure information — whether evidence it already
  possesses (reviewer findings) or could possess (acceptance output)
  changes its behavior — is the gap this experiment targets.
- *Not claimed:* that better feedback would have changed the P4-C
  outcome. That is one hypothesis among several (P4-D's prompt result
  offers an alternative explanation), and P6 examines the scoping layer
  separately.

**From P4-D** ([r2 report](../runs/2026-10-14-p4d-r2-b2-prompt-ab-final.md)):

- *Verified (§3):* specification clarity affected outcomes — one
  clarifying sentence in the operator prompt moved b2 from 0/3 (condition
  A) to 3/3 (condition B) pass, with failures tracking the targeted tests
  exactly.
- *Verified (§2, §4):* controlled A/B methodology isolates a single
  variable: byte-level prompt delta, frozen identity digests re-verified
  per slot, mechanical cross-slot configuration equality, fail-closed
  outcome classes, snapshot-based recovery of a lost measurement.
- *Not claimed:* that this proves feedback works. P4-D delivered
  clarifying information *before* the run; whether delivering failure
  information *after* a failure improves recovery is the untested
  question here.

## 3. Hypothesis

**H-FB (hypothesis, not fact):** structured failure feedback improves
recovery success compared with retrying without structured feedback —
attempts that receive the prior failure's precise evidence (failing test
names, assertion output, reviewer findings, error details, previous
diff) fix the targeted defects more often than attempts that receive only
a normal retry instruction.

Mechanism (part of the hypothesis, not established): the agent recovers
because it receives information about the previous failure instead of
rediscovering the issue.

**Alternative explanations that would also explain a positive result**
(each is a rival hypothesis, pre-declared so a win for H-FB is not
over-read):

- **A1 — Information volume:** the treatment adds content to the prompt;
  longer/more detailed prompts may help regardless of whether the added
  content is failure-specific.
- **A2 — Clearer instructions:** the feedback addendum may act as
  instruction clarification (closer to the P4-D-r2 prompt effect) rather
  than as feedback-loop behavior.
- **A3 — Task choice:** the result may be an artifact of the seed task's
  failure mode; a task whose failure is trivially fixable from its
  message would show "recovery" for any information-rich retry.

H-FB is supported only to the extent these alternatives do not better
explain the outcome (see §10).

## 4. Experimental Conditions

Two conditions, differing only in the independent variable (§5).

### 4.1 Control condition (C) — no structured failure feedback

Flow:

1. Attempt 1: agent team works on the task under the standard frozen
   prompt (normal retry instruction only).
2. Failure occurs (measured `code_correctness=false` under the frozen
   grader).
3. Attempt 2 (recovery attempt): agent receives the **normal retry
   instruction only** — no structured failure artifacts, no test names,
   no assertion output, no reviewer findings, no diffs. Any knowledge of
   the failure comes only from what the standard retry instruction
   provides.

Note: P4-C/P4-D-r2 never executed a retry (frozen `max_attempts=1`).
This pre-registration therefore does **not** claim the control flow
reflects observed system behavior; the control itself is a new
configuration requiring the identity freeze in §11.

### 4.2 Treatment condition (T) — structured failure feedback

Same flow, except attempt 2 receives a **structured failure-feedback
addendum**. Candidate feedback artifacts (the exact final set and format
must be frozen before execution — see §11 freeze items):

- failed acceptance test names (test-name granularity, from
  `acceptance/rN.json`);
- assertion output for the failing tests (from `acceptance/rN.log`);
- reviewer findings from the failed attempt, with their P0/P1/P2
  classifications as recorded;
- error messages (runner/runtime errors surfaced in records);
- previous diff information (the prior attempt's `diff.patch` or
  equivalent snapshot-derived diff).

Constraints on the feedback payload:

- It is **derived from already-graded evidence of the seed failure**,
  not authored commentary; a human or fresh model must not paraphrase or
  editorialize the failure.
- Every artifact that enters the prompt is **hash-pinned** in the
  identity manifest (r2 lesson: snapshot files are self-sufficient,
  hash-pinned measurement inputs).
- The payload contains task materials and failure evidence only — no
  evaluation reports, adjudications, or hints at the "expected" answer
  beyond what the failure evidence itself states.

### 4.3 Optional third arm (proposal only)

If the primary result is positive, a **length-matched neutral-text arm**
(prompt padded with task-relevant neutral content to match the treatment
prompt length) can be added — by a new pre-registration amendment, not
this document — to separate A1 from H-FB.

## 5. Independent Variable

The independent variable is **availability and format of structured
failure feedback** between attempt 1 and attempt 2.

Only this factor changes between conditions. Everything in §6 is a
controlled variable, frozen with digests and re-verified per slot
([methodology §3.1](../methodology.md)).

## 6. Controlled Variables

Frozen per methodology §3.1, in an identity manifest, verified before
every slot:

| Layer | Freeze |
| --- | --- |
| Task definition | Task selected per §7; definition/prompt/fixture/acceptance-suite digests pinned (reference stack: b2 v2 suite v2, fixture v1 digests) |
| Prompt templates | Team template digests pinned; identical standard prompt in both conditions; the treatment addendum's exact bytes frozen before execution |
| Model/provider | `p35-glm-5-3-flash-https` (GLM-5.3-Flash via `api.hyper-op.com`, all tiers) — or whatever variant is current at authorization; frozen in the identity either way |
| Runtime | Frozen binary/wrapper/CA digests (Baseline v2 pattern, `aaffd340…`/`0cb74fb6…`/`69ff0cf2…`), including the frozen 90 s Orchestrator decision timeout |
| Evaluator | Grader `p1.1`, acceptance runner `p2.5.0`, queue/collector/variant digests — byte-pinned; never changed mid-experiment |
| Sandbox | `bwrap` (bubblewrap 0.11.1) and its environment pins |
| Agent architecture | coding-team v6 (orchestrator + coder + reviewer) — identical in both conditions; this experiment varies **information**, not topology |
| Execution budget | `max_attempts`, `attempt_timeout`, `max_duration`, fresh queue root per slot, no silent retries — identical across conditions |

**Explicit limitation:** token usage and API cost are currently **not
measured** (`tokens`, `api_cost` = `not_measured`,
[metrics §3](../metrics.md)). Budget equality between conditions is
therefore an **unverified assumption**; the treatment adds prompt
content, so its input-token budget is by construction larger. This must
be restated verbatim in the analysis when results are reported.

## 7. Task Selection

Candidate task criteria (proposal — the final choice is frozen in the
identity manifest before execution, not decided here):

- Known failure modes: a task whose standard-prompt failure signature is
  already characterized, so recovery can be measured against a known
  baseline.
- Recovery is meaningful: the failure is plausibly fixable given the
  failure evidence (not a missing-capability wall), and success is
  objectively gradeable by the frozen suite.
- Not saturated in either direction: a task that always passes needs no
  recovery; a task that always fails regardless of information cannot
  detect the effect.

**Candidate seed task:** `b2-inventory-merge` v2 — cited here only as a
*possible* seed, **not a final decision**. Supporting facts (verified):
its failure signature under the standard prompt is known and
deterministic (P4-C: 0/3, always the same two tests; P4-D-r2 condition
A: 0/3, same tests), and the P4-D-r2 result shows the gap is closable
when the right information is present. Risks that disqualify or
re-weight it: the A3 concern (a failure the clarified prompt already
demonstrated to be information-fixable may be too easy for feedback to
distinguish itself from generic clarity); and its now extensive history,
which makes blind conditions harder to maintain. Alternatives to
evaluate during task selection: b3 (b3 passed 3/3 in P4-C — usable only
with an induced/known failure mode, or not at all), or another task from
the benchmark pool with a documented failure signature. Final selection
must be justified in the pre-execution identity freeze and must not be
changed after the first slot launches.

## 8. Metrics

Terminology per [metrics.md](../metrics.md).

**Primary**

- `code_correctness` per condition (measured, frozen grader; functional
  samples only, [methodology §4](../methodology.md)).
- Recovery success rate ([metrics §2](../metrics.md)): fraction of
  attempt-1 functional failures converted to `code_correctness=true` at
  attempt 2, per condition.

**Secondary**

- Wall-clock time per attempt (`pipeline_started` → `pipeline_completed`
  from `events.jsonl`; measured, weak proxy only).
- Number of iterations: orchestrator dispatches and revision rounds per
  attempt, from `events.jsonl` (partially derivable; no normalized
  metric defined).
- Human intervention count: **NOT CURRENTLY MEASURED**
  ([metrics §4](../metrics.md)); will be reported as `not_measured`,
  not omitted.

**Reliability (reported separately, never merged into correctness —
methodology §4):** class-2 (execution reliability) and class-3
(infrastructure) rates per condition, with counts.

**Marked unavailable:** token usage and API cost — **NOT CURRENTLY
MEASURED**. Declared per §6; no imputation, no approximation.

## 9. Experimental Design

- **Status: exploratory study.** Sample sizes below bound effect size
  coarsely; results may support or weaken H-FB but cannot establish it.
  A confirmatory claim requires a larger, separately pre-registered
  study.
- **Recommended sample size (proposal only, not an executed decision):**
  5 recovery pairs per condition — i.e. 5 attempt-1/attempt-2 pairs in
  C and 5 in T (20 slots total including attempt-1 slots, assuming one
  attempt-1 per pair is shared per condition). Rationale: P4-D-r2
  detected its effect at n=3 because it was maximal; a feedback effect,
  if real, is expected to be noisier. The final number is fixed in the
  pre-execution review and then frozen.
- **Repeated runs per condition:** attempt-1 slots may be shared across
  conditions (a failing attempt 1 seeds both a C attempt-2 and a T
  attempt-2) **only if** the pairing scheme is pre-registered; the
  default design is independent pairs per condition to avoid any shared
  seed biasing both arms identically.
- **Identity freeze requirement:** before any slot runs, a frozen
  identity manifest (§11) is created; per-slot configuration equality is
  mechanically verified against it, including across interruptions
  (r2's two-run-root continuity is the validated pattern).
- **Slot order:** frozen and interleaved (C, T, C, T, …) in the
  pre-execution amendment; no reordering after launch.
- **Outcome classes:** methodology §4 taxonomy applies unchanged;
  infrastructure failures stop the run fail-closed and are never merged
  into functional results.
- **Seeding rule:** feedback payloads derive only from attempt-1 slots
  of this study (no pooling across experiments — methodology §3.2);
  using preserved artifacts from P4-C/r2 as seed material would require
  an explicit, hash-pinned amendment.

## 10. Falsification Criteria

H-FB is **not supported** if any of the following hold:

- **No advantage:** the feedback condition's recovery success rate does
  not exceed the control condition's on the same frozen task (T ≤ C).
- **Information-amount confound not excluded:** the improvement
  disappears or materially shrinks when controlling for information
  amount (e.g. under the length-matched arm of §4.3, or when the control
  is given equivalent non-failure content in an amended pre-registration).
- **Task dependence only:** the effect appears on one task only and does
  not replicate on a second task with a different failure mode — the
  result then depends on task choice (A3), not on the feedback mechanism.
- **Signature unchanged:** T attempt-2 slots fail the same tests with
  the same failure signature as their seeds — feedback delivered, no
  behavioral change.
- **Displaced failure:** T attempt-2 slots fix the targeted tests but
  newly fail previously passing tests at a rate control does not show —
  recovery quality is worse even if the targeted metric improves.
- **Scoping override:** T attempt-2 reviewers/orchestrator again scope
  the defect out despite explicit feedback (P4-C pattern) — H-FB
  unsupported at the recovery layer; the finding redirects to P6.

Any of these outcomes is itself a valid, publishable result under this
pre-registration; null and negative outcomes are reported, not buried.

## 11. Evidence Requirements

Per [methodology §5](../methodology.md) and the P4-D-r2 evidence
standard. Before execution, the following must exist and be hash-pinned
in the frozen identity manifest:

- **Frozen identity manifest** — all §6 layers with SHA-256 digests,
  identity file itself hashed; per-slot configuration equality enforced
  mechanically.
- **Prompt/spec hashes** — standard prompt, treatment feedback-addendum
  bytes, task definition, fixture, acceptance spec/suite digests.
- **Evaluator hashes** — grader, acceptance runner, queue, collector,
  variant sources, preserved durably (P4-C preservation lesson: pinned
  evaluator sources must outlive `/tmp`).
- **Task hashes** — task definition and all task-relative artifacts.
- **Snapshot preservation** — per-round hash-pinned snapshots for every
  attempt (execution and measurement as independent artifact chains).
- **Event stream preservation** — `events.jsonl` per attempt plus
  supervisor decision logs; `pipeline_started`/`pipeline_completed`
  bounds recorded per slot.
- **Grading artifacts** — `result.json`, acceptance `rN.json`/`rN.log`,
  grader `report.md`, per-test counts; `not_measured` recorded with
  reasons, never imputed.

During execution: no silent retries; infrastructure failures stop the
run fail-closed; recovery, if needed, happens in a separate
provenance-marked artifact chain with original failure records preserved
byte-identical (r2 slot-1 pattern). After execution: original reports
are never relabeled; corrections go in addenda.

## 12. Risks and Open Questions

**Risks**

- **Information-volume confound (A1):** the treatment prompt is longer
  and richer; improvement may reflect added information rather than
  feedback-specific value. Mitigation: §4.3 length-matched arm (separate
  amendment) if the primary result is positive; otherwise the confound
  is reported explicitly with the result.
- **Learning/sequence effects across repeated attempts:** repeated runs
  on the same task by the same model may drift (provider-side caching,
  latent contamination). Mitigation: interleaved frozen slot order;
  attempt-2 slots seeded from different attempt-1 slots; no cross-run
  conversation state.
- **Task selection bias (A3):** a seed task with an easily diagnosable
  failure favors the treatment. Mitigation: pre-registered task choice
  with the criteria in §7; falsification criterion 3 (task dependence).
- **Insufficient telemetry:** without token/cost capture, budget
  equality is unverifiable and efficiency conclusions are
  wall-clock-only. Declared limitation (§6); a telemetry runtime change
  is out of scope here and would need its own pre-registration.
- **Reviewer scoping override:** as in P4-C, the scoping layer may
  neutralize feedback (a delivered finding still classified
  out-of-scope). Possible outcome, not a flaw; redirects to P6.
- **Control condition is novel:** no prior run exercised attempt-2
  retry under this program's frozen configuration; control-condition
  behavior itself is unverified until the first C slots complete.

**Open questions (to resolve before launch, in the pre-execution
amendment)**

1. Final task selection (b2 vs alternative), with justification against
   the §7 criteria and the A3 risk.
2. Exact feedback payload: which artifacts from §4.2 enter the prompt,
   their formatting, and their source-record digests.
3. Whether attempt-1 failures seed attempt-2 within the same study
   (default) or from preserved prior-run artifacts (requires explicit
   hash-pinned amendment).
4. Whether the 90 s Orchestrator decision timeout and
   `max_attempts=1`-derived parameters require adjustment for a
   two-attempt flow — any change is a declared deviation in the
   pre-execution amendment, never a mid-run edit.
5. Pairing scheme for shared attempt-1 seeds (if used instead of the
   default independent-pair design).
6. Whether to pre-register the length-matched third arm now (amendment)
   or conditionally after a positive primary result.

---

## 13. Standing rules recap

1. Nothing executes without user authorization; billable slots are
   separately approved.
2. Pre-registration is frozen before launch; deviations require a dated
   amendment, never silent edits.
3. Outcome classes are disjoint; infrastructure failures are never
   functional data.
4. Reports separate verified facts, hypotheses, interpretations, and
   proposals; `not_measured` is reported as `not_measured`.
