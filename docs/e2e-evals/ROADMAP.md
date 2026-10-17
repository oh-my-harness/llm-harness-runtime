# E2E Evaluation Roadmap — Agentic Coding Team

- **Created:** 2026-10-14
- **Status:** PROPOSALS ONLY. Nothing in §3 (Proposed experiments) is
  authorized, pre-registered, or executed. Sections §1–§2 record implemented,
  completed facts; §3 is forward-looking design intent that must go through
  the established process (dated pre-registration → user authorization →
  frozen identity file → supervised execution) before any run.
- **Inputs:** completed P4-C A/A baseline
  ([report](runs/2026-10-13-p4c-aa-baseline-v2.md)) and completed P4-D-r2
  prompt A/B ([report](runs/2026-10-14-p4d-r2-b2-prompt-ab-final.md));
  pre-registrations in `/home/liyufeng/p4b-benchmark/ab-prompt-v1/`.

---

## 1. Completed experiments and validated capabilities

| Experiment | Design | Outcome | Capability validated |
| --- | --- | --- | --- |
| P4-C `p4c-aa-baseline-v2` (2026-10-13) | A/A repeatability: one frozen configuration, 6 slots, 2 tasks (b2, b3) | All 6 slots completed with measured `code_correctness`; b2 0/3 (8/10 each), b3 3/3 (8/8 each) | **Pipeline repeatability**: identical configuration fingerprint across all slots; zero configuration drift; identical failure signature replicated 3/3 (same two acceptance tests); fail-closed supervision and frozen grader produce stable, attributable measurements |
| P4-D-r2 `p4d-r2-b2-prompt-ab` (2026-10-14) | A/B: one changed factor (operator prompt bytes), 3+3 interleaved slots | 6/6 valid functional samples; A 0/3 pass, B 3/3 pass; slot 1's acceptance measurement recovered separately after a transient bwrap EAGAIN | **Sensitivity**: the pipeline detects the effect of a single-byte-level prompt change (failures track the targeted behavior exactly); **measurement recoverability**: hash-pinned snapshots permit regenerating acceptance measurements without re-running agents; **cross-root continuity**: per-condition configuration equality enforced across an interrupted run |

Established facts both experiments rest on (frozen, digest-pinned): Baseline
v2 runtime (`aaffd340…`), bwrap sandbox, grader `p1.1`, runner `p2.5.0`,
coding-team v6 template, GLM-5.3-Flash variant `p35-glm-5-3-flash-https`,
supervisor decision table with no silent retries.

Known gaps carried across both experiments (not fixed, recorded per
pre-registration): `api_cost`, `tokens`, `human_attention`,
`event_stream_integrity` all `not_measured`; n=3 per cell is exploratory.

## 2. Open research questions for Agentic Coding Team

Ranked by the evidence gaps the completed experiments left open:

1. **Does the clarified-prompt benefit generalize beyond this task/prompt
   pair?** P4-D-r2 showed B > A on b2 only, n=3. Unknown whether the gain
   survives new tasks, larger specs, or prompts with multiple clarifications.
2. **Does the coordination failure mode recur elsewhere?** In all three P4-C
   b2 slots, reviewers independently detected the defect but scoped it out
   (P2), and the orchestrator forbade fixing it in the one revision round.
   P4-D-r2 showed a prompt fix bypasses this; it did not test whether the
   scoping layer itself can be improved (better review verdict semantics,
   scope-escalation rules).
3. **Do reviewer verdicts actually influence revision outcomes?** The only
   observed revision round (P4-C slot 5) was consumed by a docstring-only
   change with `revision_improvement = 0`. Whether review feedback ever
   produces behavioral fixes is unmeasured.
4. **Is role separation (orchestrator/coder/reviewer) load-bearing?** All
   members run the same model; whether the multi-role structure outperforms
   a single-agent loop at equal model and budget is untested.
5. **What is the reliability envelope?** Class-1 rate was 6/6 in r2 but r1
   aborted on a frozen 90 s orchestrator-decision timeout and slot 1 of r2
   hit a transient sandbox failure. Sample sizes are far too small to bound
   infrastructure and workflow failure rates.
6. **Can efficiency be measured at all?** Cost/token/attention telemetry is
   unmeasured; wall-clock duration is the only efficiency signal currently
   available.

## 3. Proposed experiments (NOT executed, NOT authorized)

Each proposal follows the r2 pattern: dated pre-registration, frozen identity
file, one changed factor, decision-table supervision, no silent retries.
Order below reflects evidence value; nothing here commits resources.

### 3.1 Failure feedback loop

**Question:** when a slot fails acceptance, can a subsequent attempt that is
shown the precise failing evidence (test names, assertion output, diff)
convert failure into pass — i.e., is the team's bottleneck information or
capability?

- **Hypothesis (H-FB):** an attempt seeded with the prior attempt's exact
  acceptance failures fixes the targeted defects more often than a fresh
  attempt with the standard prompt; if the failures persist unchanged, the
  bottleneck is capability/scoping, not information.
- **Independent variable:** presence/absence of a structured failure-feedback
  addendum to the operator prompt (prior round's failing test names +
  assertion output + snapshot diff). Everything else frozen.
- **Controlled variables:** all r2 pins unchanged — task definitions, suite
  v2, fixture, template, variant/model/provider, binary/wrapper/CA,
  evaluators, supervisor parameters, bwrap sandbox, max_attempts=1.
- **Metrics:** primary `code_correctness` per condition (measured, grader
  p1.1); secondary: per-test acceptance counts, `revision_improvement` where
  r2 rounds exist, wall-clock duration; reliability: class-1 rate.
- **Evidence needed:** ≥5 repetitions per condition (up from n=3 — r2 showed
  deterministic replication is possible in fewer slots only when the effect
  is maximal); per-slot decision-table outcome; unchanged baseline digest
  cross-check; pre-registered test-name-level pass criteria; infrastructure
  failures reported separately and never merged into the correctness metric.
- **Design notes:** use b2 (known-failing under A) as the seed task; the
  feedback arm consumes a prior completed slot's graded evidence, so the
  feedback condition must define exactly which artifacts enter the prompt and
  hash-pin them (r2's recovery package shows snapshot files suffice). Falsify
  H-FB if feedback-condition pass rate ≤ no-feedback baseline on the same
  frozen task.

### 3.2 Reviewer independence / bias

**Question:** does the reviewer's verdict track the code, or the scope
framing it receives? P4-C showed three reviewers each detecting the same
defect yet classifying it P2/out-of-scope; whether that was prompt-driven
scoping or role-driven deference is unresolved.

- **Hypothesis (H-REV):** reviewer findings and blocking decisions are
  materially influenced by the orchestrator's scope framing, independent of
  code quality: identical defective snapshots reviewed under different scope
  statements produce different verdict classifications.
- **Independent variable:** review-prompt scope framing only (e.g. "fix is
  limited to X" vs "all README/docstring-stated behaviors are in scope"),
  applied to frozen snapshots. Two designs, in escalating order:
  1. **Offline (no agent run):** replay review of the preserved P4-D-r2 A
     slots' r1 snapshots (known-defective, graded 8/10) under each framing;
     measure verdict classification. Cheapest; no agent execution.
  2. **Live:** full slots with the review-prompt framing as the single
     changed factor.
- **Controlled variables:** frozen snapshot digests (design 1) or all r2 pins
  (design 2); reviewer model/template fixed; defect ground truth fixed by
  using already-graded snapshots (the two failing tests are the defect
  oracle).
- **Metrics:** reviewer verdict class (P0/P1/P2, blocking/non-blocking);
  detection rate of the known defect; false-positive rate against known-good
  snapshots (P4-D-r2 B-slot r2 snapshots as controls); inter-run verdict
  agreement.
- **Evidence needed:** recorded reviewer outputs per framing arm with
  byte-identical snapshot inputs (manifest digests cited in each record);
  enough frozen defective/defect-free snapshot pairs to separate framing
  sensitivity from detection ability (≥3 pairs); pre-registered verdict
  rubric so classification is not post-hoc.
- **Falsification:** verdict distribution independent of framing (same
  detection and classification under both framings) → scope framing does not
  bias review.

### 3.2b (conditional) Orchestrator scope-escalation

Only if H-REV shows framing dependence: test whether an explicit
scope-escalation rule ("reviewer may re-open scope when a finding contradicts
agent-visible task materials") changes final acceptance outcomes. Same
skeleton as 3.2 design 2, with the escalation rule as the independent
variable and end-to-end `code_correctness` as the primary metric. This
directly targets the P4-C coordination finding (§5.4 of the baseline report)
rather than only the reviewer layer.

### 3.3 Architecture variants

**Question (open question 4):** is the three-role structure worth its
coordination overhead?

- **Hypothesis (H-ARCH):** at equal model, task set, and budget, the
  multi-role team and a single-agent loop differ in measured
  `code_correctness`; direction unknown a priori (coordination can help via
  review or hurt via scoping — both observed in P4-C).
- **Independent variable:** team architecture only — variants to prepare:
  (a) baseline coding-team v6 (orchestrator+coder+reviewer), (b)
  single-agent loop (one member, no review stage), (c) coder+reviewer without
  orchestrator (fixed two-step). One variant per comparison run; change one
  at a time.
- **Controlled variables:** everything else pinned as in r2 — both b2 and b3
  task definitions, suites, fixtures, prompts (b2 uses the clarified prompt
  so the known defect does not confound), variant/model/provider, binary,
  evaluators, parameters. Prompt-clarification factor must be held at the
  clarified (B) prompt for b2 across all arms, per P4-D-r2's adjudication.
- **Metrics:** primary `code_correctness` per task per arm; secondary:
  revision rounds consumed, wall-clock duration, dispatch counts (already
  captured in events.jsonl); reliability: class-1 rate per arm.
- **Evidence needed:** fresh frozen identity file per architecture variant
  with template digests; ≥5 repetitions per arm per task (coordination
  effects are expected to be noisier than the r2 prompt effect, and b2/b3
  now have known-difficulty grounding); acceptance records per round;
  architecture-variant digests in the identity file so cross-slot equality
  checking applies within arms.
- **Prerequisites (unchanged from r2's gaps):** token/cost telemetry is
  effectively required here — an architecture comparison without equal-budget
  verification cannot attribute differences to structure vs spend. Either
  instrument usage capture first or state budget-equality as an unverified
  assumption in the pre-registration.
- **Falsification/decision rule:** if measured pass rates are equal across
  variants within repetition limits, the added coordination layer is not
  validated by outcomes and simpler variants should be the default in future
  benchmarks.

---

## 4. Cross-cutting requirements for all proposals

- **Pre-registration before execution**, as in
  `ab-prompt-v1/PRE-REGISTRATION-r2.md`: explicit outcome classes
  (functional / reliability / infrastructure-uncertain), frozen slot order,
  no silent retries, fresh queue root per slot.
- **Snapshot-first design:** every attempt must preserve hash-pinned
  snapshots so measurements remain recoverable (validated by the r2 recovery).
- **Do not pool** across experiments: P4-C, P4-D-r1, P4-D-r2, and any future
  run contribute samples only within their own pre-registered study.
- **Metrics gaps** (`api_cost`, `tokens`, `human_attention`,
  `event_stream_integrity`) remain open until instrumented; any experiment
  depending on budget equality must either add telemetry or declare the gap.
- **Infrastructure-class failures are never functional data.** The r1 abort
  and the r2 slot-1 bwrap EAGAIN both demonstrated the fail-closed pattern;
  future pre-registrations should keep the stop-on-infrastructure rule and
  add measurement-layer resilience (e.g. acceptance-runner retry/backoff) as
  a separately pre-registered runtime change.
