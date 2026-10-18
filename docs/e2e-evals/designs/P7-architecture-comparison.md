# P7 Design — Architecture Comparison

- **Status:** PROPOSAL — **NOT PRE-REGISTERED, NOT AUTHORIZED, NOT
  EXECUTED.** Nothing here commits resources or implies approval.
  Execution requires the full lifecycle in
  [methodology §2](../methodology.md).
- **Type:** research design document, not an implementation plan.
- **Inputs:** completed P4-C A/A baseline
  ([report](../runs/2026-10-13-p4c-aa-baseline-v2.md)) and completed
  P4-D-r2 prompt A/B
  ([report](../runs/2026-10-14-p4d-r2-b2-prompt-ab-final.md)).
- **Claim labels:** verified facts, hypotheses, and proposals are
  separated per [methodology §1.3](../methodology.md).

---

## 1. Research Question

Which Agent Team architecture provides the best tradeoff between
capability, reliability, efficiency, and human attention?

This is a **tradeoff** question, not a winner question. The four axes are
the comparison axes of [methodology §1.1](../methodology.md); an
architecture that maximizes capability while demanding more coordination
overhead or human attention may be worse overall. **No variant is assumed
to win in advance** — including the current V0 orchestrator topology. The
direction of the outcome is genuinely unknown:

**Verified facts that motivate the uncertainty (P4-C):**

- The three-role structure produced a coordination failure: all three
  reviewers detected the same defect but scoped it out, and the one
  revision round was consumed by a docstring-only change with
  `revision_improvement = 0` (P4-C §5.4). Coordination can *hurt*.
- The same structure also produced deterministic, attributable
  measurements and a replicated failure signature across 3/3 identical
  slots (P4-C §5.1) — structure may help via review, as reviewed
  self-correction is the theoretical argument for the role split.

**Untested (from ROADMAP open question 4):** whether the multi-role
structure outperforms simpler topologies at equal model and budget is
unmeasured. Whether review feedback ever produces behavioral fixes is
unmeasured (only one revision event observed in P4-C, n=1).

## 2. Hypothesis

**H-ARCH (hypothesis, not fact):** at equal model, task set, and budget,
the multi-role team and simpler topologies differ in measured
`code_correctness`; direction unknown a priori — coordination can help via
review or hurt via scoping, both observed in P4-C (as verified behaviors;
the *explanation* is interpretation).

Secondary hypotheses (exploratory):

- **H-ARCH-REL:** reliability profiles differ by topology (single-agent
  may have fewer handoff-induced stalls; multi-role may catch more
  pre-grading via review — both unverified).
- **H-ARCH-EFF:** without coordination overhead, single-agent variants
  use fewer turns and less wall-clock time per attempt (unverified;
  token/cost equality is not measurable today — §4).

## 3. Independent Variable

**Architecture topology only.** Candidate variants, prepared one
comparison per run (methodology: one changed factor at a time):

| Variant | Topology | Notes |
| --- | --- | --- |
| **(a) V0 baseline** | orchestrator + coder + reviewer (coding-team v6) | Current frozen architecture |
| **(b) Single agent** | one member, no review stage | Minimal topology; tests whether the coordination layer is load-bearing |
| **(c) Coder + reviewer** | fixed two-step, no orchestrator | Isolates the orchestrator's contribution |

Comparisons are pairwise, one independent variable each (e.g. a-vs-b,
then a-vs-c), never a multi-arm simultaneous change without a fresh
pre-registration.

## 4. Controlled Variables

Every identity layer from [methodology §3.1](../methodology.md) pinned;
the *only* delta between arms is the topology:

- **Equal task difficulty:** the same task set for all arms — both b2 and
  b3 (P4-C grounded their difficulty: b2 known-failing under the original
  prompt, b3 known-passing). For b2, the prompt factor must be held at
  the **clarified (B) prompt** across all arms, per the P4-D-r2
  adjudication — otherwise the known defect confounds the comparison.
- **Equal model:** `p35-glm-5-3-flash-https` (GLM-5.3-Flash,
  `api.hyper-op.com`), all tiers.
- **Equal budget:** identical `max_attempts`, `attempt_timeout`,
  `max_duration` per arm. **Explicit measurement gap:** token and API-cost
  equality **cannot currently be verified** — `tokens` and `api_cost` are
  `not_measured` ([metrics §3](../metrics.md)). An architecture
  comparison without equal-budget verification cannot attribute
  differences to structure vs spend. Either usage capture is instrumented
  first (a separately pre-registered runtime change — not part of this
  design), or budget equality is declared as an unverified assumption in
  the pre-registration.
- **Equal evaluator:** grader `p1.1`, acceptance runner `p2.5.0`, frozen
  queue/collector digests, unchanged across arms.
- Same runtime binary, sandbox, decision table, no silent retries, frozen
  interleaved slot order.
- **New frozen identity file per variant**, with the variant's team
  template digests, so per-arm cross-slot configuration equality is
  mechanically enforced (r2 pattern).

## 5. Metrics

Per [metrics.md](../metrics.md), all four axes; per-axis status stated
honestly:

| Axis | Metrics | Status |
| --- | --- | --- |
| **Correctness** | `code_correctness` per task per arm (primary); per-test acceptance counts | **Measured** |
| **Reliability** | Completion rate; class-2/class-3 failure rates per arm (reported separately, never merged — methodology §4) | Measurable from existing taxonomy; sample sizes must be large enough to say anything |
| **Efficiency** | Wall-clock time (measured, weak proxy); agent turns / dispatch counts (derivable from `events.jsonl`); **token usage, API cost, retries: NOT CURRENTLY MEASURED** | Mixed — see gap in §4 |
| **Human attention** | Interventions, approval/clarification requests, review time | **NOT CURRENTLY MEASURED** ([metrics §4](../metrics.md)); declared, not omitted |
| **Secondary** | Revision rounds consumed, `revision_improvement` where r2 rounds exist | Partially derivable |

Comparison output is a per-axis table with explicit `not_measured` cells —
never a single scalar "winner" score that hides unmeasured axes.

## 6. Proposed Experimental Design

**Proposal. NOT EXECUTED.**

- **Conditions:** pairwise arms from §3 (start with a-vs-b; a-vs-c as the
  second pre-registration if the first is informative).
- **Tasks:** b2 (clarified prompt, known-difficulty) + b3 (known-passing
  baseline), both in every arm — arms must be compared on both tasks, not
  pooled.
- **Sample size:** ≥5 repetitions per arm per task (ROADMAP §3.3: at n=3
  the r2 effect was detectable only because it was maximal; coordination
  effects are expected to be noisier). Exploratory status; a claim that
  one topology is *better* requires a confirmatory follow-up with larger
  n.
- **Slot order:** frozen, interleaved across arms (r2 pattern).
- **Outcome classes:** methodology §4 taxonomy applies unchanged;
  infrastructure failures stop the run fail-closed and are reported
  separately.
- **Analysis rule:** no pooling across arms' tasks, no pooling across
  experiments; per-task per-arm aggregates only.

## 7. Falsification / Decision Rule

- If measured pass rates are equal across variants within repetition
  limits (on both tasks), the added coordination layer is **not validated
  by outcomes**; simpler variants become the default for future
  benchmarks unless reliability or quality metrics (when measurable)
  favor the larger topology. This is the ROADMAP §3.3 decision rule,
  unchanged.
- If a simpler variant matches V0 on correctness but shows *worse*
  reliability (more class-2 failures), the tradeoff is unresolved and the
  comparison is reported as such — no forced verdict.
- If results differ by task (e.g. one topology wins on b2 but not b3),
  topology choice may be task-dependent; that is a finding, not noise,
  and must be reported per-task.

## 8. Risks

- **Prompt-differences confound:** different topologies may need
  different role prompts, so "architecture" and "prompting" are entangled.
  Mitigation: pin all shared prompt content; pre-register per-variant role
  prompts as part of the identity; if a variant requires substantive
  prompt changes, treat the run as topology+prompt jointly and say so.
- **Insufficient telemetry:** without token/cost capture, equal-budget is
  an unverified assumption; efficiency-axis conclusions are provisional
  (wall-clock only) until instrumented. This is the single largest threat
  to attribution.
- **Different failure modes across topologies:** a single-agent variant
  fails differently (no review stage → no class of review-caused scoping;
  also no review-caused catching). Class counts are then not directly
  comparable in kind; report failure-mode distributions, not just rates.
- **Small-n noise:** coordination effects are expected to be subtle; at
  exploratory n, absence of evidence is not evidence of equivalence. State
  the bound, not "equal."
- **Known-difficulty asymmetry:** b2's history (adjudicated spec,
  clarified prompt) makes it sensitive but unusual; b3 may saturate at
  pass for all arms. If both tasks saturate or both floor, the comparison
  is uninformative — detectable early from the first interleaved slots.

---

## 9. Pre-execution checklist (proposal)

1. Decide telemetry prerequisite: either pre-register a token/cost
   capture runtime change first, or declare budget equality unverified.
2. Dated pre-registration per comparison: variant definitions with
   template digests, task set + clarified-b2 rule, sample size, outcome
   classes, decision table.
3. Frozen identity file per variant.
4. User authorization (billable slots; estimated 2 arms × 2 tasks × 5
   reps = 20 slots minimum for the first comparison).
