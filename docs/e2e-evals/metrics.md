# E2E Evaluation Metrics — Agentic Coding Team

- **Status:** Framework v1 — definitions only. **No telemetry is
  implemented by this document.** Any metric marked NOT CURRENTLY MEASURED
  stays unmeasured until a separately pre-registered runtime change adds
  capture (see [methodology](methodology.md) §2).
- **Companion:** methodology and result taxonomy in
  [methodology.md](methodology.md).
- **Current measurement baseline:** P4-C and P4-D-r2 recorded
  `code_correctness`, per-test acceptance counts, and wall-clock duration;
  `api_cost`, `tokens`, `human_attention`, and `event_stream_integrity`
  were `not_measured` in every slot.

---

## 1. Correctness Metrics

The capability axis. Measured by the frozen grader (`p1.1`) against the
frozen acceptance suite.

| Metric | Definition | Status |
| --- | --- | --- |
| `code_correctness` | Per-slot boolean, measured from independent unittest assertions executed by the frozen grader in the sandbox. Primary metric in all experiments to date. | **Measured** (grader p1.1) |
| Acceptance pass rate | Per-test counts from the frozen suite (e.g. 8/10 vs 10/10), at test-name granularity. Finer than the binary metric; identifies *which* behaviors fail. | **Measured** (acceptance runner p2.5.0) |
| Requirement adherence | Whether the delivered behavior satisfies the task specification (task definition + README/docstring contract), as distinct from passing the pinned suite. Captures prompt/spec coverage gaps (e.g. the P4-C b2 scope dispute) that the suite alone cannot. | **Not measured** — no rubric or grader exists yet; define before first use |

Rules:

- Computed over **functional samples only** (methodology §4.1).
- Test-name-level results are the defect oracle; report which tests fail,
  not just how many.
- Values are frozen at grading time and never relabeled
  ([README](README.md) convention).

## 2. Reliability Metrics

Can the system deliver a graded result every time, and is the pipeline
itself a negligible contributor?

| Metric | Definition | Status |
| --- | --- | --- |
| Completion rate | Fraction of submitted slots reaching class 1 (functional sample). P4-D-r2: 6/6. | **Measurable** from existing outcome classes |
| Infrastructure failure rate | Fraction of slots classified class 3 (infrastructure/provider). Always reported separately, never merged into correctness aggregates. | **Measurable** from existing taxonomy; sample sizes so far too small to bound (r1 abort, r2 slot-1 bwrap EAGAIN) |
| Recovery success rate | Fraction of measurement-layer failures recovered from preserved snapshots without re-executing the agent workflow. | **Measurable** where recovery is attempted (r2 slot 1: 1/1); no aggregate meaning until more events exist |
| Execution reliability failure rate | Fraction of slots classified class 2 (workflow incomplete, timeout, measurement unavailable). | **Measurable** from existing taxonomy |
| Event stream integrity | Whether the per-attempt event stream is complete (no lost intermediate events). | **NOT CURRENTLY MEASURED** (grader records valid start/terminal rows only; cannot prove no loss) |

## 3. Efficiency Metrics

**NOT CURRENTLY MEASURED.** All metrics in this section are framework
definitions only. Wall-clock duration is the only efficiency signal
currently available; it is a weak proxy (confounded by provider latency
and sandbox jitter) and must not substitute for the metrics below.

| Metric | Definition | Status |
| --- | --- | --- |
| Wall-clock time | `pipeline_started` → `pipeline_completed` from `events.jsonl` per attempt. | **Measured** (reported per slot; weak proxy only) |
| Token usage | Input/output tokens consumed across all team members and tiers. | **NOT CURRENTLY MEASURED** (`tokens` = `not_measured`) |
| API cost | Monetary spend across all model calls. | **NOT CURRENTLY MEASURED** (`api_cost` = `not_measured`) |
| Agent turns | Number of orchestrator dispatches, coder/reviewer turns, and revision rounds per attempt (dispatch counts already exist in `events.jsonl`). | Partially derivable from event streams; no normalized metric defined |
| Retries | Count of retries at any layer (agent self-corrections, supervisor-level retries). Supervisor rule today is no silent retries; any future retry mechanism must be pre-registered and counted. | **NOT CURRENTLY MEASURED** (no retry mechanism in the frozen configuration) |

Consequence (carried from the roadmap): any experiment whose claim depends
on budget equality (e.g. architecture comparisons) must either add token/
cost telemetry first or declare budget equality as an unverified
assumption in its pre-registration.

## 4. Human Attention Metrics

Aligned with the **Human-at-the-right-step** principle: humans should
intervene at high-leverage decision points, and the system should demand
as little human attention as possible otherwise. This axis measures the
attention cost of the system, not just its autonomy.

**NOT CURRENTLY MEASURED.** Framework definitions only.

| Metric | Definition |
| --- | --- |
| Human interventions | Count and type of manual actions taken by a human during execution (reroutes, manual fixes, scope overrides). |
| Approval requests | Number of points where the system pauses for human approval, and their leverage (does each request gate a genuinely consequential step, or routine work?). |
| Clarification requests | Number of times the system asks the human to disambiguate the task or spec. Low is only good if the work is still correct — clarification avoidance that produces confident wrong answers is not efficiency. |
| Human review time | Time humans spend reviewing outputs, artifacts, or reports to reach a decision. |

Design intent: capture these from existing artifacts where possible
(chat records, supervisor logs) rather than adding live instrumentation;
the `human_attention` `not_measured` reason in the grader explicitly notes
interventions are not fully captured today.

## 5. Quality Metrics

Long-term properties of produced code beyond "does it pass the pinned
suite". All are future-facing; none has a rubric or grader yet.

| Metric | Definition | Status |
| --- | --- | --- |
| Maintainability | Readability, structure, and locality of the produced change (diff size vs behavior change, test coverage of new code). | **NOT CURRENTLY MEASURED** |
| Design adherence | Conformance to the task's stated design constraints and existing codebase conventions. | **NOT CURRENTLY MEASURED** |
| Regression risk | Likelihood the change breaks adjacent behavior; observable proxies include snapshot diff surface and changes to previously passing tests. | **NOT CURRENTLY MEASURED** (proxy: previously passing tests, when rerun) |
| Documentation quality | Whether docstrings/comments/README updates match actual behavior (P4-C slot 5 showed a docstring-only revision being consumed on a non-behavioral finding — documentation claims are already behavior-adjacent evidence). | **NOT CURRENTLY MEASURED** |

These matter for comparisons over many experiments; they must not be
retrofitted onto completed runs as scores.

## 6. Comparison Rules

For any two conditions, configurations, or systems to be compared:

### 6.1 An experiment must specify, before execution (in the pre-registration)

1. **Independent variable** — the single changed factor, at byte-level
   precision (P4-D-r2: operator prompt bytes only).
2. **Controlled variables** — every identity layer from
   [methodology §3.1](methodology.md) pinned with digests, re-verified per
   slot.
3. **Sample size** — with an explicit statement of whether it is
   confirmatory or exploratory (n=3/condition in P4-D-r2 was declared
   exploratory; H1 "supported, not proven").
4. **Evaluation criteria** — the primary metric, pre-registered support
   and falsification criteria, and per-test pass thresholds where
   applicable.

### 6.2 Do not

- **Mix infrastructure failures into functional results.** Correctness
  aggregates are computed over class-1 slots only; infrastructure and
  reliability failures are reported separately with counts
  ([methodology §4](methodology.md)).
- **Compare experiments with different frozen identities without
  explanation.** Runs with different task versions, prompts, evaluator
  digests, binaries, templates, or models are not directly comparable. If
  such a comparison is made, the report must enumerate exactly which
  identity layers differ and argue why the comparison is still meaningful.
- **Pool samples across experiments.** P4-C, P4-D-r1, P4-D-r2, and future
  runs contribute samples only within their own pre-registered study.
- **Retroactively relabel outcomes** when evaluators or specs change.
- **Treat unmeasured metrics as measured.** `not_measured` is reported as
  `not_measured`; never impute, never approximate silently, never omit the
  axis from a comparison table.
