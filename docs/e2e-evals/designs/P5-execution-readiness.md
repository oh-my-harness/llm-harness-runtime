# P5 Execution Readiness Review

- **Status:** REVIEW ONLY. This document identifies unresolved decisions
  before P5 execution. **It makes no decisions.** Nothing here authorizes
  execution or resolves open questions on anyone's behalf; every open
  decision in §7 requires explicit human choice, recorded in a dated
  pre-execution amendment to
  [the P5 pre-registration](../preregistrations/2026-10-p5-failure-feedback-loop.md).
- **Inputs:** [methodology](../methodology.md),
  [metrics](../metrics.md), [P5 design](P5-failure-feedback-loop.md),
  [P5 pre-registration](../preregistrations/2026-10-p5-failure-feedback-loop.md),
  P4-C ([report](../runs/2026-10-13-p4c-aa-baseline-v2.md)), P4-D-r1
  ([abort report](../runs/2026-10-13-p4d-r1-abort.md)), P4-D-r2
  ([report](../runs/2026-10-14-p4d-r2-b2-prompt-ab-final.md)).
- **Claim labels:** verified facts, proposals, and open decisions are
  labeled per [methodology §1.3](../methodology.md). Where an item is a
  proposal it is marked **[P]**; open decisions are marked **[D]** and
  left unresolved.

---

## 1. Task Selection

### 1.1 Verified facts

- b2 (`b2-inventory-merge` v2): 0/3 pass under the original prompt in
  P4-C (always `test_case_duplicates_inside_one_dict_add` +
  `test_report_total_duplicates`, 8/10) and 0/3 in P4-D-r2 condition A
  with the identical signature; 3/3 pass under the clarified prompt
  (condition B). Failure signature is deterministic and known; the gap
  is demonstrably closable when the right information is present.
- b3 (`b3-rate-limiter` v2): 3/3 pass, 8/8, in P4-C. No known failure
  mode exists.
- Both tasks are gradeable by the frozen grader p1.1 / runner p2.5.0
  stack; b2's specification was adjudicated (in-scope, additive
  everywhere) during the P4-D adjudication step.
- No other task in this program has a documented, replicated failure
  signature.

### 1.2 Candidates, advantages, disadvantages

| Candidate | Advantages | Disadvantages |
| --- | --- | --- |
| **b2 with original prompt** | Known deterministic failure mode; recovery objectively gradeable; deepest evidence trail; closest to existing identity files (lowest re-freeze cost) | Contamination: the task is now heavily adjudicated and documented; "blind" conditions are harder to maintain because the failure and its fix are extensively described in repo docs. A3 risk: the failure is already proven information-fixable (P4-D-r2 B), so feedback may not separate from generic clarity. History bias: the model may have seen related material via provider-side context |
| **b2 with clarified prompt** | Eliminates the known prompt-scoping defect; any remaining failure would be a new, less-characterized one | No known failure signature — contradicts the pre-registration's task criterion (known failure modes); b2 passed 3/3 under B in r2, so attempt-1 failures may not occur at all |
| **b3** | Clean, uncontaminated; passes 3/3 | No failure mode exists; unusable as-is. Would require either an induced/seeded failure (a new task variant — itself a design decision) or waiting for a naturally failing slot, neither of which exists today |
| **A different task from the benchmark pool** | No contamination; fresh failure mode would strengthen generality (falsification criterion 3 in pre-reg §10) | Failure mode undocumented; a task-scouting step (running attempt-1 slots to characterize failures) would consume billable slots before the main study and risks post-hoc task selection — the exact A3 bias the pre-registration warns about |

### 1.3 Contamination risks (verified basis + open items)

- **Verified:** b2's failure signature, adjudication history, and fix
  are extensively documented in this repository's reports. Whether the
  model provider has any exposure to this documentation is unknowable
  from here; the P4-C/r2 replication of identical failure signatures
  suggests no practical contamination *of the failure mode itself* so
  far, but this is an absence of evidence, not evidence of absence.
- **[D] Open:** whether history contamination weighs enough to prefer a
  fresh task with a characterization phase, or b2 for its known
  signature. Not decided here.

## 2. Failure Feedback Payload

### 2.1 Information candidates (from pre-registration §4.2; all derivable from existing artifact types)

| Candidate artifact | Source | Derivable today? |
| --- | --- | --- |
| Failed test names | `acceptance/rN.json` per-test records | Yes |
| Assertion output | `acceptance/rN.log` | Yes |
| Reviewer findings + verdict classes | team `chat.sqlite3` review messages | Yes (P4-C reports cite them) |
| Error messages | runner/runtime records in the evaluation run dir | Yes (r2 §4 shows recorded error text) |
| Previous diff | snapshot `diff.patch` (r0→rN) | Yes |

### 2.2 What must be frozen before execution

- **[D]** The exact subset of §2.1 artifacts entering the prompt, their
  formatting (template text around them), and ordering.
- **[D]** The byte-level content of the feedback addendum itself — it
  must be frozen (digest-pinned) before the first T slot; per
  methodology §3.1, the addendum is part of the prompt identity.
- **[D]** Source-record binding: which specific attempt-1 run's records
  seed each attempt-2 slot, and the digests of those records, frozen
  before the corresponding T slot launches (pre-registration §9
  seeding rule).
- Verified constraint already fixed: payload must be evidence-derived
  only, no authored commentary, no adjudication/report content
  (pre-registration §4.2).

### 2.3 Possible confounds

- **Verified:** treatment prompt is longer than control by construction;
  input-token budget equality is unverifiable (`tokens` not measured).
- **[D] Open:** whether to pre-register the length-matched neutral-text
  arm now or conditionally after a positive primary result
  (pre-registration §12 question 6).
- **[D] Open:** how much reviewer-finding context to include —
  reviewer findings include scope classifications (P2/out-of-scope);
  passing scope classifications into the feedback may *reinforce* the
  P4-C scoping behavior rather than fix it, or may surface the defect
  more forcefully. Both directions are possible; the choice interacts
  with the payload decision above.

## 3. Retry Protocol

### 3.1 Verified facts constraining the design

- The frozen configuration has **never executed a two-attempt flow**:
  `max_attempts=1` in all P4-C/P4-D runs; the control's retry instruction
  behavior is unverified (pre-registration §4.1, §12).
- P4-D-r1 verified: a frozen **90 s Orchestrator first-decision timeout**
  is baked into the Baseline v2 binary and fired fatally in r1 (one
  non-ACTION reply at ~55 s; `timed_out`, exit 10, class 3/2 boundary —
  recorded as an infrastructure-class outcome in the r1 report). P4-C
  never triggered it in six slots. Any two-attempt design doubles
  exposure to this deadline.
- `attempt_timeout=1800 s`, `max_duration=3600 s` are the frozen outer
  budgets; `max_duration=3600 s` cannot contain two 1800 s attempts —
  this arithmetic conflict is a verified constraint on the current
  parameter set.
- Snapshot points r0 (baseline) / r1 / r2 are collected per attempt and
  were proven recoverable measurement inputs (r2 slot-1 recovery).
- No-silent-retry rule and fail-closed stop on infrastructure failure
  are established supervision behavior (r1, r2).

### 3.2 Unresolved retry-protocol decisions

- **[D] Attempt structure:** whether attempt 2 runs as (a) a fresh slot
  in a new runtime instance seeded with the feedback addendum, or (b)
  any in-place continuation. The pre-registration default is independent
  pairs (§9); the mechanics of "attempt 2" are not yet defined against
  the actual supervisor tooling (`max_attempts` semantics under the
  frozen binary are unverified for this flow).
- **[D] Snapshot points:** whether attempt 2 needs its own r0 baseline
  (fresh workspace from the same fixture) or inherits attempt-1's final
  snapshot as its baseline. r2 recovery showed snapshot chains are the
  measurement substrate; the choice affects what "recovery" diff means
  (from-baseline vs from-failed-state).
- **[D] Timeout requirements:** whether the 90 s Orchestrator decision
  timeout and `max_duration`/`attempt_timeout` values are adjusted for
  a two-attempt flow (pre-registration §12 question 4 requires any
  change to be a declared deviation in the pre-execution amendment —
  never a mid-run edit). Note the r1 hypothesis that the timeout may be
  model-behavior-sensitive; changing it is a controlled-variable change
  and must be weighed as such.
- **[D] Retry budget:** how many attempt-2 opportunities per seed (the
  design implies one), and whether an attempt-2 infrastructure failure
  consumes the pair or is replaced under the pre-registered decision
  table (no silent retries; replacement via new dated manifest — r1
  precedent).

## 4. Metrics Readiness

### 4.1 What can currently be measured (verified from P4-C/P4-D-r2 artifacts)

| Metric | Status | Source |
| --- | --- | --- |
| `code_correctness` | Measured | Frozen grader p1.1 |
| Acceptance pass counts, test-name granularity | Measured | Acceptance runner p2.5.0 records |
| Wall-clock duration | Measured | `events.jsonl` `pipeline_started`→`pipeline_completed` |
| Orchestrator dispatches / revision rounds | Derivable | `events.jsonl` + chat records |
| Outcome classes (functional / reliability / infrastructure) | Measurable | Supervisor decision table + queue states |
| Recovery success rate | Computable | Defined over attempt-1 → attempt-2 pairs; primary metric of P5 |

### 4.2 Telemetry gaps (verified: all recorded `not_measured` in every P4-C/P4-D slot)

- `tokens` — NOT CURRENTLY MEASURED.
- `api_cost` — NOT CURRENTLY MEASURED.
- `human_attention` — NOT CURRENTLY MEASURED.
- `event_stream_integrity` — NOT CURRENTLY MEASURED.

Consequences for P5 specifically:

- Budget equality between conditions is unverifiable; the treatment
  prompt is larger by construction. Pre-registration §6 declares this;
  analysis must restate it.
- Efficiency-axis conclusions will be wall-clock-only (weak proxy).
- Retry/iteration counts are derivable from `events.jsonl` but no
  normalized metric exists; secondary status only.
- **[D] Open:** whether to pre-register a token/cost telemetry runtime
  change before P5 (a separate pre-registration per the roadmap's
  cross-cutting rules) or run without it, restating the limitation. Not
  decided here.

## 5. Identity Freeze Checklist

State per item — what exists (verified) vs what must be created before
launch ([D] items are open decisions or missing prerequisites):

| Item | State |
| --- | --- |
| Task | **[D]** Final selection open (§1.2); once chosen: definition/prompt/fixture/suite digests pinned (pattern exists: b2 v2 `099cd84e…`, suite `a36b4b3e…`) |
| Prompt | Standard prompt digest reusable from r2 (b2 original `d014886d…`, clarified `f425eb36…` — which one depends on the task decision). **[D]** Feedback addendum bytes do not exist yet; must be authored and frozen before launch |
| Model | **Ready pattern** — `p35-glm-5-3-flash-https` (`db90e75a…`) pinned in both prior identity files; re-verify at authorization |
| Runtime | **Ready pattern** — Baseline v2 binary `aaffd340…`, wrapper `0cb74fb6…`, CA `69ff0cf2…`, 90 s decision timeout included; r1 verified these re-hash from `/home/liyufeng/p4c-baseline-freeze/` |
| Evaluator | Grader `p1.1` `4325acb7…`, runner `p2.5.0` `6c7be50a…`, queue `111a494a…`, collector `44ec16a1…`, variant `596e3c3…`-era digests. **Verified risk:** the live repo has moved past two pinned evaluator files (`e2e_queue.py` and its test) — durable archive exists (`/home/liyufeng/p4c-aa/frozen-source/p4c-aa-ready-20261013`, verified `diff -r` clean, manifest self-check 13/13). Readiness review should re-verify archive integrity at freeze time |
| Sandbox | `bwrap` (bubblewrap 0.11.1); known transient EAGAIN risk — see §6 blocked-adjacent item |
| Agent architecture | coding-team v6 template tree `c7e40e0c…` — unchanged requirement across conditions |
| **Feedback artifact** | **[D] Does not exist.** The single largest missing identity item: payload subset, template, and source digests (§2.2). No execution until this is frozen |
| Execution budget | **[D]** `max_attempts`/`attempt_timeout`/`max_duration` must be re-derived for two-attempt flow (§3.2); current frozen values are internally insufficient (`max_duration` < 2 × `attempt_timeout`) |
| Identity manifest file | **[D]** New dated manifest required (r2 pattern: `p4d-r2-ab-identity.json`, hash-locked, mechanically verified per slot) |

## 6. Decision Gates

Gate definitions: **Ready** = no action needed, verified in place.
**Needs decision** = a human choice recorded in the pre-execution
amendment unblocks it. **Blocked** = a prerequisite does not exist and
cannot be created by a decision alone (requires implementation or an
experiment that has not run).

| # | Item | Gate | Why |
| --- | --- | --- | --- |
| 1 | Framework (methodology, metrics, taxonomy, comparison rules) | **Ready** | v1 complete; terminology consistent across all four docs |
| 2 | Pre-registration skeleton | **Ready** | 12 sections complete; falsification criteria and evidence requirements pre-stated |
| 3 | A/B isolation methodology | **Ready** | Validated by P4-D-r2 (single-byte factor, mechanical cross-slot equality, fail-closed classes, interruption continuity) |
| 4 | Snapshot-based measurement recoverability | **Ready** | Verified by r2 slot-1 recovery; per-attempt snapshot collection exists |
| 5 | Supervisor decision-table pattern | **Ready** | r1/r2 demonstrated fail-closed stop, no silent retries |
| 6 | Task selection | **Needs decision** | §1.2 table; user chooses b2-original / b2-clarified / b3-with-induced-failure / other (with characterization cost) |
| 7 | Feedback payload (subset + format + bytes) | **Needs decision** | §2.2; no artifact exists yet |
| 8 | Retry protocol (attempt structure, snapshot baseline semantics, timeout deviations, retry budget) | **Needs decision** | §3.2; four sub-decisions, each a pre-execution amendment item |
| 9 | Sample size finalization + slot order | **Needs decision** | Pre-registration §9 leaves the number and order to pre-execution review |
| 10 | Seeding scheme (independent pairs default vs shared attempt-1 seeds) | **Needs decision** | Pre-registration §9 default exists; deviation is a decision |
| 11 | Third (length-matched) arm timing | **Needs decision** | Now-amendment vs post-positive-result; affects slot count and analysis power |
| 12 | Token/cost telemetry | **Needs decision → conditional Blocked** | Not implementable within this framework (no runtime changes); if the study requires verified budget equality, it is **Blocked** until a separately pre-registered telemetry change exists; if declared as an unverified assumption, gate drops to Needs decision |
| 13 | Acceptance-runner retry/backoff for transient namespace failures | **Needs decision** | r2 lesson; runtime change requiring its own pre-registration; not blocking if absent (r2 recovered without it) but reduces class-3 attrition risk |
| 14 | Feedback addendum implementation (code that assembles the payload into a prompt) | **Blocked** | Explicitly out of scope per constraints: no feedback mechanisms may be implemented now. Becomes a pre-execution implementation task only after decisions in §2 are made |
| 15 | Control-condition retry instruction | **Needs decision** | The "normal retry instruction" text has never existed in this program (no prior retry); its bytes are an identity item, not a default |

## 7. Consolidated open decisions requiring human choice

1. **Task:** which seed task (§1.2). Includes whether a b2-clarified or
   other-task variant needs a characterization phase first.
2. **Feedback payload:** which artifacts, in what format, frozen as
   bytes (§2.2).
3. **Attempt structure & snapshot semantics:** fresh-slot vs
   continuation; attempt-2 baseline from fixture or from failed
   snapshot (§3.2).
4. **Timeout/budget deviations:** 90 s decision timeout exposure;
   `max_duration` vs two-attempt budget conflict (§3.1, §3.2).
5. **Pairing scheme:** independent pairs (default) vs shared seeds (§9
   of pre-registration).
6. **Telemetry:** pre-register token/cost capture first, or run with
   the declared unverified-budget assumption (§4.2 consequence).
7. **Third arm timing:** length-matched arm pre-registered now or
   conditionally later (§2.3).
8. **Authorization:** billable slot approval remains a separate,
   explicit user action regardless of all above (standing rule).

## 8. Bottom line

No execution-blocking gap exists in the framework itself: methodology,
taxonomy, evidence standards, and supervision patterns are validated and
documented. Every remaining gap is a **decision** (task, payload, retry
mechanics, budgets, telemetry scope) plus one **implementation**
prerequisite (the feedback addendum builder — intentionally out of scope
now). The readiness position is: **not ready to launch; ready to make
the decisions.**
