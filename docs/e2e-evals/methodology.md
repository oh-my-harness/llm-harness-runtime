# E2E Evaluation Methodology — Agentic Coding Team

- **Status:** Framework v1 — normative for all future E2E evaluations
  (P5/P6/P7 and beyond). This document defines methodology only; it does not
  implement telemetry or change runtime/evaluator behavior.
- **Derived from:** P4-C A/A baseline
  ([report](runs/2026-10-13-p4c-aa-baseline-v2.md)) and P4-D-r2 prompt A/B
  ([report](runs/2026-10-14-p4d-r2-b2-prompt-ab-final.md)), including the
  P4-D-r1 abort ([report](runs/2026-10-13-p4d-r1-abort.md)) as a
  fail-closed case study.
- **Companion:** metric definitions in [metrics](metrics.md).

---

## 1. Evaluation Philosophy

### 1.1 Evaluation is more than "does the code pass"

A binary pass/fail number is the least informative part of an E2E
evaluation. The goal is to compare agent systems along four axes:

| Axis | Question it answers | Current measurement status |
| --- | --- | --- |
| **Capability** | Can the team solve the task at all? (`code_correctness`, acceptance pass rates) | Measured (grader p1.1) |
| **Reliability** | Can it do so every time, and does the pipeline itself stay out of the way? (completion rate, failure classes) | Partially measured |
| **Efficiency** | At what cost — time, tokens, API spend, agent turns? | **NOT CURRENTLY MEASURED** (`api_cost`, `tokens` recorded `not_measured`) |
| **Human attention cost** | How much human intervention, approval, and review time does the system demand? ("Human-at-the-right-step") | **NOT CURRENTLY MEASURED** (`human_attention` recorded `not_measured`) |

A system that passes but requires constant babysitting, or passes only
because a sandbox hiccup was silently absorbed into its score, is not the
same system as one that passes cleanly and autonomously. Only the
capability axis is directly comparable today; the other axes are defined in
[metrics.md](metrics.md) and must be declared `not_measured` until
instrumented — not omitted and not approximated.

### 1.2 Evidence principle: "Compress presentation, not evidence"

Summaries, tables, and aggregate rates are presentation. The underlying
evidence — raw logs, event streams, snapshot trees, grader records — must
remain complete and recoverable at its original location.

Rules:

- Raw evidence is never copied into this repository; reports link exact
  paths and record SHA-256 digests (established convention, see
  [README](README.md)).
- Every aggregate number in a report must be traceable to specific
  per-slot evidence files. A reader who distrusts the summary must be able
  to recompute it from linked artifacts.
- Summaries may be compacted, rewritten, or superseded in later documents.
  Raw evidence is immutable once recorded.

The P4-D-r2 slot-1 recovery is the worked example: the agent workflow
(chat transcripts, snapshots, diffs) and the acceptance measurement were
independent artifact chains. When bwrap failed transiently at measurement
time, the execution evidence was untouched and the measurement was
regenerated from hash-pinned snapshots — without re-running or re-paying
the agent workflow. That only works when evidence is preserved, not
compressed away.

### 1.3 Epistemic labels

Every claim in a report must be attributable to one of four categories.
Mixing them is the primary way evaluations drift into storytelling.

| Label | Meaning | Example |
| --- | --- | --- |
| **Verified fact** | Directly observed in evidence; reproducible from linked artifacts with digests | "All three A slots failed exactly `test_case_duplicates_inside_one_dict_add` and `test_report_total_duplicates` (P4-D-r2 §3.2)" |
| **Hypothesis** | Falsifiable claim with a pre-registered support/falsify criterion; never presented as settled | "H1: the clarified prompt lets the coder close the two failing tests — supported, not proven (n=3/condition)" |
| **Interpretation** | Reading of verified facts that goes beyond them; may be disputed and must be marked as such | "Reviewers scoped out a real defect because the operator prompt scoped the fix to incoming-vs-current only (P4-C §5.4)" |
| **Future proposal** | Design intent for work not yet authorized or executed; carries no evidential weight | "Add acceptance-runner retry/backoff for transient namespace failures (requires its own pre-registration)" |

P4-C §5 demonstrated why this matters: an initial interpretation (prompt/
suite mismatch → task invalid) was partially overturned by independent
review, but the graded `code_correctness` values were never relabeled.
Corrections go in addenda or new reports; recorded outcomes are frozen.

---

## 2. Experiment Lifecycle

Every future E2E experiment follows this pipeline:

```
Research Question
      ↓
Hypothesis
      ↓
Pre-registration
      ↓
Identity Freeze
      ↓
Execution
      ↓
Evidence Collection
      ↓
Grading
      ↓
Analysis
```

| Stage | Purpose | Non-negotiables |
| --- | --- | --- |
| **Research Question** | State what is unknown and worth a billable run. Phrased so a result can actually answer it. | Must be answerable with the metrics available (or explicitly declare the metrics gap, e.g. budget-equality questions before telemetry exists). |
| **Hypothesis** | A falsifiable prediction with a pre-declared support criterion and falsification criterion. | Direction and thresholds fixed before execution; "exploratory signal" sample sizes (e.g. n=3) are stated as such up front. |
| **Pre-registration** | Dated document frozen before launch: hypothesis, conditions, slot order, outcome classes, decision table, no-silent-retry rule. | Cannot be edited after execution starts; deviations require a new pre-registration. Pattern: `ab-prompt-v1/PRE-REGISTRATION-r2.md`. |
| **Identity Freeze** | Create the experiment's identity file: digests for every layer listed in §3 below. | The identity file is hash-locked; per-slot configuration equality is mechanically verified against it, including across interruptions. |
| **Execution** | Run slots in the frozen order under the pre-registered decision table. | No silent retries, no slot replacement, no relabeling; infrastructure failures trigger fail-closed stop, not improvisation. |
| **Evidence Collection** | Preserve per-slot: event streams, snapshots with manifests and file hashes, chat records, supervisor logs, decision-table outcomes. | Snapshots must be self-sufficient, hash-pinned measurement inputs (r2 lesson). Original failure records are never modified. |
| **Grading** | Frozen evaluator (pinned digests) produces `code_correctness` and per-test acceptance records. | Evaluator digests re-verified at grading time; `not_measured` is a valid recorded outcome and is reported as such, never imputed. |
| **Analysis** | Classify outcomes per the taxonomy (§4), compute primary/secondary metrics, write the report with the §1.3 epistemic labels. | Verified / hypothesis / interpretation / proposal separated; infrastructure failures reported separately; no pooling across experiments. |

---

## 3. Identity and Reproducibility

### 3.1 What must be frozen

P4-C and P4-D-r2 both demonstrated that trustworthy conclusions came from
continuous identity verification, not sample size. Every experiment must
freeze, in an identity file, with SHA-256 digests:

- **Task definition** — definition, prompt, fixture, acceptance spec/suite
  digests.
- **Prompt/specification** — exact prompt bytes per condition; the one
  changed factor is a byte-level delta (P4-D-r2: original vs clarified b2
  prompt, `d014886d…` vs `f425eb36…`).
- **Templates** — team template tree digest (coding-team v6,
  `c7e40e0c…`).
- **Model/provider** — variant digest and provider endpoints, all tiers
  (`p35-glm-5-3-flash-https`, GLM-5.3-Flash via `api.hyper-op.com`).
- **Runtime binary** — binary, wrapper, and CA bundle digests (Baseline v2:
  `aaffd340…`, `0cb74fb6…`, `69ff0cf2…`), including frozen behavioral
  parameters such as the 90 s Orchestrator decision timeout.
- **Evaluator version** — grader, acceptance runner, queue, collector,
  variant digests (grader `p1.1`, runner `p2.5.0`, …).
- **Sandbox environment** — sandbox type and version (`bwrap`,
  bubblewrap 0.11.1).
- **Configuration** — supervisor parameters (`max_attempts`,
  `attempt_timeout`, `max_duration`), configuration fingerprint, per-slot
  effective configuration digests.

### 3.2 Rules

1. **Experimental variables must be explicit.** An A/B experiment changes
   exactly one documented factor between conditions; everything in §3.1 is
   a controlled variable and is re-verified before every slot.
   Cross-slot configuration equality is enforced mechanically (all slots in
   a condition share one effective configuration digest; the A-vs-B delta
   must be only the declared factor). P4-D-r2 enforced this across two run
   roots after an interruption — identity checks must survive such seams.
2. **Historical results are not reinterpreted under changed evaluators.**
   `code_correctness` values are frozen at grading time (README
   convention). If the evaluator, grader, or task spec later changes, old
   results keep their original meaning; corrections, regradings, and
   re-adjudications go in new reports or addenda. The P4-C b2 spec dispute
   was resolved by *adjudication recorded in a new report* — the graded
   outcomes were never relabeled. Pinned evaluator sources must be
   preserved durably (see the P4-C preservation note: `/tmp` copies
   migrated to a permanent archive) so a frozen evaluator remains
   re-runnable years later.
3. **Conditions are not mixed.** Samples pool only within their own
   pre-registered study. P4-C, P4-D-r1, P4-D-r2, and future runs are never
   merged into one sample set, even when configurations are similar, and
   experiments with different frozen identities are not compared without
   an explicit explanation of what changed and why the comparison is still
   meaningful. Conditions within one experiment are never blended at
   analysis time; a slot belongs to exactly one condition.

---

## 4. Result Taxonomy

Every slot outcome is classified into exactly one of three disjoint
categories, declared in the pre-registration before execution (established
in P4-D-r1/r2 as outcome classes).

### 4.1 Functional Sample

A slot that produces a valid, measured correctness result.

Requirements:

- Execution completed (the agent workflow ran to a terminal, valid state).
- Correctness was measured by the frozen grader (a real
  `code_correctness` value exists).

Can be:

- **pass** (`code_correctness=true`)
- **fail** (`code_correctness=false`)

Both are valid data. A fail is a full measurement of the system, not a
defect of the evaluation.

### 4.2 Execution Reliability Failure

The workflow or measurement layer failed in a way attributable to the
system-under-test's operational behavior, not to its coding ability and
not to infrastructure.

Examples:

- Workflow incomplete (e.g. a frozen decision timeout stops the pipeline —
  P4-D-r1 slot 1: no Coder/Reviewer dispatch, exit 10, `code_correctness=null`).
- Measurement unavailable after recovery attempts.
- Recoverable execution issues that exhausted their pre-registered budget.

Reliability failures are themselves primary data when the research question
concerns reliability (P4-D-r1's abort *is* the documented timeout
finding), but they never contribute to correctness aggregates.

### 4.3 Infrastructure / Provider Failure

Failure of the surrounding machinery, not of the agent system.

Examples:

- Runtime failure (binary crash, wrapper failure).
- Sandbox failure (P4-D-r2 slot 1: transient bwrap
  `Creating new namespace failed: EAGAIN` on the acceptance runner).
- Provider outage (model API unavailability).

The fail-closed pattern applies: stop, preserve evidence, classify as
infrastructure, exit non-zero. Recovery (as in the r2 slot-1 snapshot-based
acceptance regeneration) happens in a separate, provenance-marked artifact
chain; the original failure record stays byte-identical.

### 4.4 Rules

- **Do not merge these categories.** Every aggregate must state which
  classes it includes. Primary correctness metrics are computed over
  functional samples only.
- **Infrastructure failure is not a functional failure.** A bwrap EAGAIN
  does not make condition B a fail; a provider outage does not make
  condition A a pass. Infrastructure-failing slots are excluded from
  functional aggregates and reported separately with counts (so the reader
  can see the attrition).
- A recovered slot keeps both records: the original infrastructure-failure
  record (untouched) and the recovered measurement (marked
  `recovered=true`), as in P4-D-r2 slot 1.
- Reliability vs infrastructure classification must be defensible from
  evidence; if the cause is genuinely unresolved, record it as unresolved
  (P4-D-r1's causality was recorded as unresolved, not guessed).

---

## 5. Evidence Preservation

### 5.1 Required evidence per experiment

| Artifact | Contents | Why |
| --- | --- | --- |
| **Identity manifest** | Frozen digests for all §3.1 layers; identity file hash | Makes any slot re-verifiable against its declared configuration |
| **Hashes** | Per-slot configuration fingerprint, prompt/spec/suite/fixture digests, snapshot manifest `file_hashes` | Detects drift and binds every measurement to exact bytes |
| **Snapshots** | Per-round snapshot trees (`snapshot.json`, `files/`, `diff.patch`), hash-pinned | The measurement substrate; enables regeneration without re-running agents |
| **Event streams** | `events.jsonl` per attempt (`pipeline_started` → `pipeline_completed`), supervisor logs, decision-table outcomes | Duration, dispatch counts, supervision decisions, timeline of the incident if any |
| **Result records** | `result.json` per grading run, `metadata.json` per attempt, `not_measured` reasons where applicable | The official, frozen outcome record |
| **Grader output** | Acceptance `rN.json`/`rN.log`, per-test counts, grader `report.md` | The correctness evidence itself, test-name-level |

Reports link exact paths and SHA-256s; raw artifacts stay at the original
run roots.

### 5.2 Why snapshot preservation matters

Snapshots are what make the measurement layer independent of the execution
layer. The validated example is **P4-D-r2 slot 1 acceptance recovery**:
after the agent workflow completed successfully, both acceptance-runner
invocations failed instantly on a transient host-level bwrap namespace
failure (`EAGAIN`, exits in ~10–20 ms, no tests run). Because the round
snapshots (`r1/files`, `r2/files`) were preserved with verified manifests,
the recovery could:

1. re-hash the snapshot trees against their manifests (exact match),
2. re-verify the frozen spec/prompt/suite digests byte-for-byte,
3. re-execute the exact recorded command in the same sandbox shape against
   the preserved snapshots,
4. produce 10/10 pass in both rounds — with the recovered records marked
   `recovered=true` and the original failure logs untouched.

No agent workflow was re-executed, no API cost was re-paid, no slot was
retried, and no result was relabeled. Under the "compress presentation,
not evidence" principle, a compressed pipeline that had kept only "slot 1:
completed, measurement failed" would have lost a functional sample
entirely (or worse, invited an informal rerun). Design rule going forward:
**every snapshot is a self-sufficient, hash-pinned measurement input**;
if a measurement can be regenerated from preserved artifacts, it must be,
rather than re-executing the workflow.

---

## 6. Summary of standing rules

1. Compare systems on capability, reliability, efficiency, and human
   attention cost; declare unmeasured axes rather than dropping them.
2. Compress presentation, not evidence; label every claim
   (verified / hypothesis / interpretation / proposal).
3. Follow the eight-stage lifecycle; pre-register before executing; freeze
   identity before launching.
4. One changed factor per comparison; cross-slot equality mechanically
   verified; no pooling across experiments or mixing of conditions.
5. Classify every slot as functional / reliability / infrastructure;
   never merge classes; never convert infrastructure failure into
   functional data.
6. Preserve snapshots and event streams so any measurement is recoverable
   without re-running agents.
