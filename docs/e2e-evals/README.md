# E2E Evaluation History

Durable record of end-to-end (E2E) agent-team evaluation runs. This is the
canonical experiment index; individual reports live in `runs/`. Evidence itself
stays outside this repository (large raw artifacts are referenced by path and
SHA-256, never copied here).

## Conventions

- One report per pilot/experiment: `runs/<date>-<experiment-id>.md`.
- `code_correctness` values are frozen at grading time. Never relabel a
  recorded outcome retroactively; corrections go in a new report or addendum.
- A/A runs repeat one fixed configuration (same task set, team template,
  model, evaluator digests). They measure repeatability, not architecture
  differences. A/B runs change exactly one documented factor.
- Every report must name: configuration fingerprint, frozen binary/evaluator
  SHA-256s, model, sandbox mode, task definitions, exact evidence paths, and a
  clear split of verified observations vs interpretations vs open decisions.

## Experiment index

| Date | Experiment ID | Type | Status | Key results | Report | Follow-up |
| --- | --- | --- | --- | --- | --- | --- |
| 2026-10-14 | `p4d-r2-b2-prompt-ab` (P4-D-r2) | b2 prompt A/B (clarified vs original operator prompt; one changed factor) | Completed; 6/6 valid functional samples | A: 0/3 pass (`code_correctness=false`, 8/10 acceptance each, always the two case-duplicate tests); B: 3/3 pass (10/10 each; slot 1 via separately recovered acceptance measurement after transient bwrap EAGAIN). H1 supported, not proven (n=3/condition, exploratory) | [2026-10-14-p4d-r2-b2-prompt-ab-final](runs/2026-10-14-p4d-r2-b2-prompt-ab-final.md) | Larger pre-registered study for a reliable prompt-clarification claim; acceptance-runner retry/backoff for transient namespace failures (requires new pre-registration) |
| 2026-10-13 | `p4d-r2-b2-prompt-ab` (P4-D-r2) | Re-dated b2 prompt A/B (conditions/order/variables identical to r1) | Prepared, NOT executed; launch requires user authorization | New identity `p4d-r2-ab-identity.json` (`7d986608…bf8c`); preregistration adds explicit outcome classes (functional / reliability / infrastructure-uncertain) and records r1 causality as unresolved | [PRE-REGISTRATION-r2](/home/liyufeng/p4b-benchmark/ab-prompt-v1/PRE-REGISTRATION-r2.md), identity `p4d-r2-ab-identity.json` (`7d986608…`) | Awaiting explicit user authorization; billable 6×≤1800 s |
| 2026-10-13 | `p4d-b2-prompt-ab` (P4-D-r1) | b2 prompt A/B, first live attempt | Aborted in slot 1 (B): infrastructure failure | Slot 1 (B) `blocked`/`timed_out`: frozen 90 s Orchestrator first-decision timeout; one non-ACTION reply at ~55 s (EndTurn/ToolUse warning; causality unresolved); no Coder/Reviewer dispatch; `code_correctness=null`; exit 10; 5 slots never attempted; evidence preserved | [2026-10-13-p4d-r1-abort](runs/2026-10-13-p4d-r1-abort.md) | P4-D-r2 prepared (new dated identity `p4d-r2-ab-identity.json`, unchanged variables); launch requires separate authorization |
| 2026-10-13 | `p4d-b2-adjudication-ab` | b2 spec adjudication + original-vs-clarified prompt A/B | Prepared, NOT executed | b2 spec ruled in-scope (additive everywhere); A/B definitions prepared with checksums; n=3/condition exploratory | [2026-10-13-p4d-b2-adjudication-ab](runs/2026-10-13-p4d-b2-adjudication-ab.md) | User decisions: A/B execution authorization + supervisor order limitation (b2-only) |
| 2026-10-13 | `p4c-aa-baseline-v2` | A/A repeatability (single configuration, 6 slots) | Completed; b2 spec dispute open | b2 0/3 passed (8/10 each); b3 3/3 passed (8/8 each) | [2026-10-13-p4c-aa-baseline-v2](runs/2026-10-13-p4c-aa-baseline-v2.md) | Independent b2 spec adjudication; possible original-vs-clarified prompt A/B; metrics gaps; read-only monitoring interface |

## Roadmap

Forward-looking evaluation roadmap (completed-experiment summary, open
research questions, and proposed-but-not-executed experiments):
[ROADMAP](ROADMAP.md).

## Adding a future run

1. Run the pilot; keep all raw evidence in its original run directory.
2. Create `runs/<YYYY-MM-DD>-<experiment-id>.md` with objective, frozen
   configuration identity, official outcomes, durations/dispatches, analysis
   (verified / disputed / hypothesis / undecided), and evidence paths.
3. Prepend one row to the table above. Statuses: `planned`, `running`,
   `completed`, `completed — dispute open`.
4. Do not copy raw evidence files into this repository; link exact paths.
