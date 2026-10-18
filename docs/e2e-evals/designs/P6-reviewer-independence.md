# P6 Design — Reviewer Independence / Bias

- **Status:** PROPOSAL — **NOT PRE-REGISTERED, NOT AUTHORIZED, NOT
  EXECUTED.** Nothing here commits resources. Stage 2 (live) additionally
  requires its own authorization even if Stage 1 is approved. Lifecycle
  per [methodology §2](../methodology.md).
- **Type:** research design document, not an implementation plan.
- **Inputs:** P4-C baseline
  ([report](../runs/2026-10-13-p4c-aa-baseline-v2.md)), P4-D-r2
  ([report](../runs/2026-10-14-p4d-r2-b2-prompt-ab-final.md)).
- **Claim discipline:** this document **does not claim reviewer bias
  exists.** Whether reviewer verdicts track code quality or framing is the
  open research question. Per [methodology §1.3](../methodology.md),
  observed P4-C behavior is a verified fact; any explanation of it is an
  interpretation or hypothesis.

---

## 1. Research Question

Does reviewer behavior (findings, severity classification, blocking
decisions, recommended actions) depend on framing, role instructions, or
available context — or only on defect evidence?

**Motivation (verified facts, P4-C §5.1/§5.4):** in all three P4-C b2
slots, each Reviewer independently detected the residual case-collision
defect and classified it P2 (non-blocking, out of scope). In slot 5 the
Orchestrator explicitly prohibited fixing it, and the docstring-only
revision produced `revision_improvement = 0`.

**What is NOT claimed ([INFERENCE]/interpretation only):** that the
reviewers were biased, wrong, or defective. Under the then-unadjudicated
specification, "out of scope" was a defensible classification; the b2
scope dispute was later adjudicated (see P4-D-r2 §1), which changed the
*evaluation* of the classification, not the reviewer behavior itself. The
question is whether such classifications are stable under changed framing
or are framing-dependent.

## 2. Hypothesis

**H-REV (hypothesis, not fact):** reviewer decisions may depend on
framing/context, not only on defect evidence — identical defective
snapshots reviewed under different scope framings produce materially
different verdict classifications, even though the code and its defects
are byte-identical.

Competing hypothesis (null direction): reviewer verdicts track the defect
evidence alone; framing changes wording but not classification or blocking
decisions.

## 3. Independent Variables

Examined one at a time (methodology §3.2 rule 1: one changed factor per
comparison):

| Variable | Variants | Notes |
| --- | --- | --- |
| **Review instruction framing** | (a) "the fix is limited to X" (incoming-vs-current scoping, as in the P4-C b2 prompt); (b) "all README/docstring-stated behaviors are in scope" | The r2-adjudicated reading makes (b) the broader contract |
| **Available context** | (a) code diff only; (b) diff + task README/docstring; (c) diff + full task materials | Tests whether verdicts change when the broader contract is visible |
| **Role definition** | (a) current reviewer role template; (b) role text that permits scope re-opening when a finding contradicts agent-visible task materials | This variant is the scope-escalation probe (ROADMAP §3.2b); conditional on Stage 1 showing framing dependence |

## 4. Controlled Variables

- **Same snapshot:** hash-pinned snapshot digests byte-identical across
  all framing arms (the P4-D-r2 A-slot r1 snapshots are known-defective,
  graded 8/10; the two failing tests are the defect oracle).
- **Same defect ground truth:** defect presence/absence fixed by using
  already-graded snapshots — defect oracle = the two failing acceptance
  tests (`test_case_duplicates_inside_one_dict_add`,
  `test_report_total_duplicates`).
- **Same evidence:** identical review inputs apart from the independent
  variable; reviewer model/template fixed; defect-free control snapshots
  from P4-D-r2 B slots (10/10 graded) for false-positive measurement.
- **Same model/provider:** `p35-glm-5-3-flash-https` (GLM-5.3-Flash).
- **Same evaluator/frozen layers:** all methodology §3.1 identity pins;
  Stage 1 needs no runtime execution at all.
- **Same review round position:** reviews of r1 snapshots only, so round
  context does not vary.

## 5. Metrics

Per [metrics.md](../metrics.md):

| Metric | Definition | Status |
| --- | --- | --- |
| Defect detection rate | Fraction of framing arms in which the known defect is identified at all | Derivable from recorded reviewer outputs |
| Severity classification agreement | Distribution of verdict classes (P0/P1/P2, blocking/non-blocking) across arms for the same snapshot; inter-arm agreement | Derivable |
| Recommended action | Whether the reviewer recommends fixing, scoping out, or escalating — and whether the recommendation survives framing changes | Derivable |
| Consistency across conditions | Same reviewer input under different framings: identical verdict text/class or not | Derivable |
| False-positive rate | Findings raised on known-good (B-slot) snapshots, per framing | Derivable |
| Inter-run verdict agreement | Same framing repeated: baseline reviewer stochasticity, needed to distinguish framing effects from run-to-run noise | Derivable |

All are offline-computable from reviewer output records; no new telemetry
required. Human attention metrics are not applicable at Stage 1 (no live
run).

## 6. Proposed Experiments

**Both stages are proposals. NOT EXECUTED.**

### Stage 1 — Offline snapshot re-review (no agent execution)

Replay review of preserved, already-graded snapshots under each framing
arm. Uses the P4-D-r2 A-slot r1 snapshots (known-defective, 8/10) as
defective inputs and B-slot r2 snapshots (10/10) as known-good controls.

- Cheapest design: no agent run, no billable slots; reviewer outputs
  recorded per framing with manifest digests cited in each record.
- Requires a **pre-registered verdict rubric** so classification is not
  post-hoc (methodology §2: pre-registration before analysis).
- Needs ≥3 defective/defect-free snapshot pairs to separate framing
  sensitivity from detection ability (ROADMAP §3.2).
- Outcome gate: Stage 2 is justified only if Stage 1 shows framing
  dependence beyond the measured reviewer-stochasticity baseline.

### Stage 2 — Live experiment (only if Stage 1 justifies it)

Full team slots with the review-prompt framing as the single changed
factor, measuring end-to-end `code_correctness` (and, in the escalation
variant, whether an explicit scope-escalation rule changes acceptance
outcomes). Same skeleton as P5's live design: frozen identity, ≥5
repetitions per arm, outcome classes, fail-closed decision table.

Stage 2 carries P5-style live costs and risks (provider variance, class-2/
class-3 failures) and is a separate pre-registration — never bundled.

## 7. Falsification Criteria

- **Framing effect unsupported** if reviewer decisions (verdict class,
  blocking status, recommended action) remain identical — beyond the
  measured repeat-run stochasticity — across different framings on
  byte-identical snapshots. H-REV weakened; reviewer behavior is
  evidence-driven at the tested framings.
- **Framing effect supported** if the same defective snapshot yields
  materially different classifications across framings while the
  null-framing repeats are stable. Note: even then, this establishes
  framing *sensitivity*, not "bias" — whether a framing-dependent verdict
  is wrong requires the adjudicated ground truth, which exists only for
  the b2 case.
- If Stage 1 shows no framing dependence, Stage 2 must not run as
  designed; the P4-C out-of-scope classifications stand as a scoping-layer
  property worth studying by other means (e.g. P5's feedback results).

## 8. Risks

- **Offline/live divergence:** a reviewer seeing only a frozen snapshot
  (Stage 1) may behave differently from a reviewer inside a live team
  conversation with orchestrator context. Stage 1 results may not
  transfer; that is why Stage 2 is separately gated.
- **Post-hoc classification risk:** without a pre-registered rubric,
  verdict classes could be assigned to fit the narrative. Mitigation:
  rubric frozen in the pre-registration.
- **Known-answer leakage:** reviewers may infer the "expected" answer
  from artifacts that hint at adjudication history. Mitigation: review
  inputs contain only the original task materials and code, not reports
  or adjudications.
- **Reviewer stochasticity confound:** a single verdict per cell cannot
  distinguish framing effects from model noise; the repeat-run baseline
  is mandatory, which multiplies offline review count.
- **Small-n trap:** P4-D-r2 demonstrated maximum separation at n=3
  because the effect was maximal; framing effects are expected to be
  subtler. Exploratory conclusions only.
- **Ground truth limited to b2:** defect ground truth is defined by the
  adjudicated b2 spec; generalization to other task types is out of scope
  for this design.

---

## 9. Pre-execution checklist (proposal)

1. Dated pre-registration (Stage 1): framing variants, snapshot digests,
   verdict rubric, repeat-run baseline plan, outcome classes.
2. Hash-pinned snapshot manifest per reviewed artifact.
3. User authorization for Stage 1 (offline; no billable agent slots) and
   a separate authorization for Stage 2.
4. If framing dependence is found: the scope-escalation variant (ROADMAP
   §3.2b) becomes a candidate follow-up, itself requiring its own
   pre-registration.
