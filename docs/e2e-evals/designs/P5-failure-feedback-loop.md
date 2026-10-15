# P5 Design — Failure Feedback Loop

- **Status:** PROPOSAL — **NOT PRE-REGISTERED, NOT AUTHORIZED, NOT
  EXECUTED.** Nothing here commits resources or implies approval. Execution
  requires the full lifecycle in [methodology §2](../methodology.md):
  dated pre-registration → user authorization → frozen identity file →
  supervised execution.
- **Type:** research design document, not an implementation plan.
- **Inputs:** completed P4-C A/A baseline
  ([report](../runs/2026-10-13-p4c-aa-baseline-v2.md)) and completed
  P4-D-r2 prompt A/B
  ([report](../runs/2026-10-14-p4d-r2-b2-prompt-ab-final.md)).
- **Claim labels:** per [methodology §1.3](../methodology.md), this
  document separates verified facts, hypotheses, and proposals.

---

## 1. Research Question

Does structured failure feedback improve agent recovery compared with
retrying without feedback?

Operationalized: when a slot has failed acceptance, does a subsequent
attempt that is shown the precise failing evidence (failing test names,
assertion output, reviewer findings, prior diff) convert failure into pass
more often than a fresh attempt given no structured failure information?

This targets the bottleneck question: is the team's limiting factor
**information** (it never saw the right failure detail) or **capability /
scoping** (it saw the issue and still did not fix it)? The P4-C
coordination finding is the motivating verified fact — see §2.

## 2. Verified facts this design builds on

- **Verified (P4-C §5.4):** in all three b2 slots, reviewers independently
  detected the residual case-collision defect and classified it P2
  (non-blocking, out of scope); the one revision round was explicitly
  scoped to exclude it and produced `revision_improvement = 0`.
- **Verified (P4-D-r2 §3):** a single prompt change (adding one sentence
  about case-equivalent duplicates) moved b2 from 0/3 to 3/3 pass.
  Failure information delivered *before* the run changed the outcome.
- **Verified (P4-D-r2 §4):** hash-pinned snapshots made the slot-1
  acceptance measurement recoverable without re-running agents; snapshot
  artifacts are self-sufficient measurement inputs.
- **Unverified hypothesis this experiment tests (from P4-C §5.6 and
  ROADMAP §3.1 H-FB):** that feeding a prior attempt's failure evidence
  into a new attempt closes the same gap.

## 3. Hypothesis

**H-FB (hypothesis, not fact):** structured failure feedback improves
recovery because the agent receives information about the previous failure
instead of rediscovering the issue; attempts seeded with the prior
attempt's exact acceptance failures fix the targeted defects more often
than fresh attempts with the standard prompt.

**Falsification direction (pre-stated):** if the feedback condition's pass
rate is ≤ the no-feedback baseline on the same frozen task, or if failures
persist unchanged (same test names, same failure mode) under feedback, the
bottleneck is capability/scoping, not information — H-FB weakened.

## 4. Independent Variable

Feedback availability and form, delivered as an addendum to the operator
prompt. Exactly one factor changes between conditions.

| Arm | Content |
| --- | --- |
| **Control (C):** | no structured failure information; standard prompt only |
| **Treatment (T):** | structured failure-feedback addendum: failing acceptance test names + assertion output; reviewer findings from the failed attempt (with their P0/P1/P2 classifications); error details; the previous attempt's diff (or a pointer to the preserved snapshot diff) |

Design note (from ROADMAP §3.1): the feedback arm consumes a prior
completed slot's graded evidence. The design must define **exactly which
artifacts enter the prompt** and hash-pin them — the r2 recovery package
demonstrates that snapshot files suffice as self-sufficient inputs. Any
artifact referenced by the feedback prompt becomes part of the frozen
identity.

## 5. Controlled Variables

All identity layers from [methodology §3.1](../methodology.md) pinned with
digests and re-verified per slot:

- Same task: `b2-inventory-merge` v2 (known-failing under the standard
  prompt — this is the seed task), suite v2, fixture v1.
- Same model/provider: `p35-glm-5-3-flash-https` (GLM-5.3-Flash,
  `api.hyper-op.com`, all tiers).
- Same runtime: frozen Baseline v2 binary/wrapper/CA, including the frozen
  90 s Orchestrator decision timeout.
- Same evaluator: grader `p1.1`, acceptance runner `p2.5.0`, frozen
  queue/collector/variant digests.
- Same agent architecture: coding-team v6 (orchestrator + coder +
  reviewer) — this experiment varies **information**, not topology.
- Same budget: `max_attempts`, `attempt_timeout`, `max_duration` identical
  across arms. **Budget equality in tokens/cost is a declared measurement
  gap:** `api_cost` and `tokens` are `not_measured`; the pre-registration
  must state wall-clock-based budget equality as an unverified assumption
  ([metrics §3](../metrics.md)).
- Same decision table, no silent retries, frozen slot order, fresh queue
  root per slot.

## 6. Metrics

Per [metrics.md](../metrics.md) terminology:

**Primary**

- `code_correctness` per arm (measured, grader p1.1; functional samples
  only per methodology §4).
- Recovery success rate ([metrics §2](../metrics.md)): fraction of seeded
  failures converted to pass under each arm.

**Secondary**

- Wall-clock time per attempt (measured; weak proxy only).
- Iterations: dispatch counts and revision rounds from `events.jsonl`
  (partially derivable today; no normalized metric defined).
- Human intervention: **NOT CURRENTLY MEASURED**
  ([metrics §4](../metrics.md)); reported as `not_measured`, not omitted.

**Reliability (reported separately, never merged into correctness)**

- Class-2 (execution reliability) and class-3 (infrastructure) rates per
  arm, per methodology §4.

## 7. Experimental Design

| Element | Design |
| --- | --- |
| Baseline condition | C: fresh attempt on b2 with the standard prompt, no failure information |
| Treatment condition | T: fresh attempt on b2 with the standard prompt + hash-pinned failure-feedback addendum from a prior completed, graded failing slot |
| Sample size | ≥5 repetitions per arm (up from the n=3 exploratory cells of P4-C/r2; ROADMAP §3.1 rationale: deterministic replication was observable in few slots only because the r2 effect was maximal — a feedback effect, if real, is expected to be noisier) |
| Status | **Exploratory.** Even at n=5, this bounds effect size coarsely; results support or weaken H-FB, they do not establish it. A confirmatory claim would require a larger pre-registered follow-up. |
| Slot order | Frozen and interleaved in the pre-registration (r2 pattern: alternate arms to decorrelate from time/provider drift) |

The feedback seed must come from an already-graded failing slot within the
same study (no pooling across experiments — methodology §3.2 rule 3), or
from a preserved prior run only if the pre-registration explicitly
declares and hash-pins that cross-experiment artifact use.

## 8. Falsification Criteria

H-FB is weakened or falsified if any of the following occur:

- T pass rate ≤ C pass rate on the same frozen task.
- T slots fail the **same tests with the same signature** as the seed
  failure (the feedback did not change behavior at all).
- T slots show the failure moving to different tests without fixing the
  targeted ones (information did not translate into a fix).
- Reviewers in T slots still scope the defect out (feedback delivered but
  the scoping layer overrides it — would point at the P6 question
  instead).

## 9. Risks

- **Prompt-length confound:** feedback may simply make the prompt longer
  or different in ways unrelated to failure information. Mitigation: a
  length-matched neutral-text control (pre-registered as an optional
  third arm) if the primary result is positive; otherwise report the
  confound explicitly.
- **Attribution ambiguity:** improvement may come from more information
  generally, not from failure feedback specifically — the treatment is
  indistinguishable from "a longer, more detailed prompt" without the
  length-matched control.
- **Seed selection bias:** if the seed failure is unusually easy, the
  feedback effect is overstated. Mitigation: pre-register the seed slot
  and its failure signature before any T slot runs.
- **Reviewer scoping override (P4-C lesson):** even with perfect feedback,
  the orchestrator/reviewer scoping layer may forbid fixing the defect, as
  it did in P4-C slot 5. This is a possible outcome, not a design flaw;
  if observed, it redirects the research to P6.
- **Budget equality unverified:** without token/cost telemetry, T's
  advantage could partly be "more input tokens." Declared as an unverified
  assumption unless telemetry is added first (separately pre-registered).

---

## 10. Pre-execution checklist (proposal)

1. Dated pre-registration: hypothesis, arms, feedback-artifact list with
   digests, slot order, outcome classes, decision table.
2. Frozen identity file (extends the r2 identity with the feedback
   addendum digests).
3. User authorization (billable slots).
4. Acceptance-runner retry/backoff for transient namespace failures is a
   separate, pre-registered runtime change — not bundled into P5
   (methodology: one changed factor).
