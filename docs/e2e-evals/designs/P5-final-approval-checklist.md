# P5 Final Approval Checklist — Failure Feedback Loop

- **Created:** 2026-10-14
- **Status:** APPROVAL PREPARATION REVIEW. **Nothing here is approved,
  pre-registered as executable, or authorized.** This document consolidates
  the P5 decision state into a single pre-approval view. It makes no
  decisions and authorizes no billable slots.
- **Inputs:** [P5 design](P5-failure-feedback-loop.md),
  [P5 pre-registration](../preregistrations/2026-10-p5-failure-feedback-loop.md),
  [P5 execution readiness](P5-execution-readiness.md),
  [P5 execution decision](P5-execution-decision.md),
  [methodology](../methodology.md), [metrics](../metrics.md),
  [ROADMAP](../ROADMAP.md), and fresh digest re-verification performed
  2026-10-14 (see §8).
- **Labeling per methodology §1.3:** **[V]** = verified fact (observed in
  evidence or re-verified on disk 2026-10-14); **[R]** = recommendation
  (proposal, not approved); **[D]** = human decision (open until the user
  explicitly chooses).

---

## 1. Recommended configuration (summary of the decision document §9 — all items [R], none approved)

| Parameter | Recommendation | State |
| --- | --- | --- |
| Task | `b2-inventory-merge` v2, **original prompt** (`d014886d…`), with a stop rule: re-verify the failure signature in the first C attempt-1 slots before any T slot launches | Decision open ([D1]) |
| Conditions | C = normal retry instruction only (no structured feedback); T = frozen feedback addendum: failed test names + assertion output only | Decision open ([D2]) |
| Architecture | coding-team v6 (orchestrator + coder + reviewer), unchanged | Ready; digest re-verified |
| Model/provider | `p35-glm-5-3-flash-https` (GLM-5.3-Flash, `api.hyper-op.com`), re-pinned at freeze | Ready; digest re-verified |
| Runtime | Frozen Baseline v2 binary/wrapper/CA incl. 90 s Orchestrator decision timeout — unchanged | Ready; digests re-verified |
| Evaluators | Grader p1.1, runner p2.5.0, queue/collector/variant digests from the frozen archive | Ready; archive re-verified 13/13 |
| Sandbox | bwrap 0.11.1 | Ready; version re-verified |
| Attempt structure | Attempt 2 = fresh slot, new runtime instance, workspace from the same fixture baseline; independent pairs (no shared seeds) | Decision open ([D3]) |
| Budget | `attempt_timeout=1800 s` unchanged; `max_duration` raised (e.g. 4000 s) to contain two attempts — declared deviation; 90 s decision timeout unchanged | Decision open ([D4]) |
| Sample size | 5 recovery pairs per condition; 20 slots; explicitly exploratory | Decision open ([D5]) |
| Slot order | Frozen, interleaved C,T,C,T,… frozen in the pre-execution amendment | Decision open ([D5]) |
| Seeding | Attempt-2 feedback derives only from same-study attempt-1 graded failures; source digests frozen per T slot | Decision open ([D6]) |
| Third arm | Not in the first study; length-matched arm only via dated amendment if the primary result is positive | Decision open ([D7]) |
| Telemetry | None added; budget equality declared an unverified assumption, restated verbatim in analysis | Decision open ([D8]) |
| Control retry-instruction bytes | Authored and byte-frozen before launch (text does not exist yet) | Decision open ([D2a]) |

---

## 2. Remaining human decisions (all [D]; each must be recorded in the dated pre-execution amendment)

| # | Decision | Recommended option | Basis |
| --- | --- | --- | --- |
| [D1] | **Seed task** — b2-original / b2-clarified / b3-with-induced-failure / other pool task | b2-original + signature-replication stop rule | Decision doc §1 |
| [D2] | **Feedback payload subset + exact bytes** — which of: test names, assertion output, prior diff, reviewer findings (with/without verdict classes) | Test names + assertion output only | Decision doc §2 |
| [D2a] | **Control retry-instruction bytes** — the "normal retry instruction" has never existed in this program; its text is an identity item | Author a minimal neutral retry instruction, byte-freeze | Readiness §6 item 15 |
| [D3] | **Attempt structure & snapshot semantics** — fresh slot vs in-place continuation; attempt-2 baseline from fixture vs from failed snapshot | Fresh slot, fixture baseline, independent pairs | Decision doc §3 |
| [D4] | **Timeout/budget deviations** — raise `max_duration` (verified conflict: 3600 < 2×1800); keep or raise the 90 s decision timeout | Keep 90 s timeout and 1800 s attempts; raise only `max_duration` | Decision doc §4 |
| [D5] | **Sample size + slot order** | 5 pairs/condition, 20 slots, interleaved C,T,C,T,… | Decision doc §5 |
| [D6] | **Seeding scheme** — independent pairs (default) vs shared attempt-1 seeds | Independent pairs | Decision doc §3; pre-reg §9 |
| [D7] | **Third (length-matched) arm timing** | Conditionally pre-registered; added by dated amendment only after a positive primary result | Decision doc §6 |
| [D8] | **Telemetry declaration** — run without token/cost capture vs pre-register telemetry first | Run without; declare budget equality unverified | Decision doc §7 |
| [D9] | **Acceptance-runner retry/backoff** — separately pre-registered runtime change; adopt before P5 or not | Not bundled into P5 (one-changed-factor rule); defer | Decision doc §8 step 4 |
| [D10] | **Authorization to execute** — billable slots; explicit user action after [D1]–[D9] and the frozen amendment | — | Standing rule |

Gate status per readiness §6: items 1–5 Ready; items 6–13, 15 Needs decision; item 12 conditionally Blocked (telemetry); item 14 (feedback-addendum builder) Blocked by design until [D2] is approved.

---

## 3. Risks if proceeding (with the recommended configuration)

**Verified foundations:**

- **[V]** Framework gates 1–5 (methodology, pre-registration, A/B isolation, snapshot recoverability, fail-closed supervision) are validated by P4-C/P4-D-r1/r2.
- **[V]** Every §1 identity pin re-verified on disk 2026-10-14: Baseline v2 binary `aaffd340…`, wrapper `0cb74fb6…`, CA `69ff0cf2…`; b2 prompt `d014886d…`, suite `a36b4b3e…`, spec `83cf11f5…`; variant `db90e75a…`; template team.json `b2063941…` and the orchestrator/coder/reviewer prompt digests all match the r2 identity manifest; bwrap 0.11.1; Python 3.14.4.
- **[V]** The frozen evaluator archive (`/home/liyufeng/p4c-aa/frozen-source/p4c-aa-ready-20261013`) re-verified 13/13 manifest entries clean today. The live repo's `tools/eval/e2e_queue.py` differs from the pin (live `8e2b9a38…` vs pinned `111a494a…`) — expected per readiness §5; execution must use the archived copies.
- **[V]** The feedback payload is derivable from existing artifacts: r2 acceptance records (`r1.json` with per-test counts, `r1.log` with `FAIL:` lines and assertion output) exist per slot, e.g. `p4d-ab/runs/r2-…/attempt-4-…-A/…/acceptance/r1.{json,log}`.
- **[V]** Known arithmetic conflict: `max_duration=3600 s` < 2 × `attempt_timeout=1800 s` (verified in the r2 identity parameters). Unfixed, the two-attempt flow cannot fit — this is the one mandatory deviation.

**Risks (accepted under the recommendation, declared in analysis):**

1. **A1 information-volume confound** — T's prompt is longer by construction; budget equality unverifiable (`tokens`, `api_cost` = `not_measured` in every prior slot). Any positive result carries this unquantified confound until the length-matched arm runs.
2. **A3 task-choice confound** — b2's failure is proven information-fixable (r2 condition B 3/3); a positive result may reflect generic clarity, not feedback. Mitigated by falsification criteria (signature-unchanged, displaced-failure, task-dependence) and [D7].
3. **90 s decision-timeout attrition** — 1 fatal event in 13 prior slots (~9%). Exposure is identical across C and T, so it does not bias the comparison; fail-closed stop + dated-manifest replacement is the established handling (r1→r2 precedent). Expected attrition must be stated in the analysis.
4. **Control condition is novel** — no prior run exercised a two-attempt flow; control behavior is unverified until the first C slots complete. The stop-rule on signature replication ([D1]) partially covers the same risk on the T side.
5. **Scoping override may neutralize feedback** (P4-C pattern): T reviewers may again classify the defect out of scope. This is a valid outcome that redirects to P6, not a design flaw.
6. **n=5/condition is exploratory** — bounds effect size coarsely; cannot confirm H-FB.
7. **Contamination/history bias on b2** — highest of all task options; provider-side exposure unknowable. Identical failure replication across six prior slots suggests no practical contamination so far (absence of evidence, not evidence of absence).
8. **Worktree drift** — the live repo has 72 uncommitted modifications/deletions (template prompts moved, `planner.md`/reviewer variants deleted, new src files untracked). None affects the run (execution uses the frozen binary + archived evaluators), but freeze-time verification must run **against the pinned artifacts**, never the live tree.

---

## 4. Risks if changing each decision

| Change | Risk introduced |
| --- | --- |
| **Task ≠ b2-original** ([D1]→(b)/(c)/(d)) | (b) b2-clarified: no known failure signature — attempt-1 failures may not occur; violates the pre-registration's own task criterion. (c) b3-induced: requires designing/validating a new failure mode (new identity items, characterization billable slots) — highest preparation cost and its own A3 risk. (d) other task: undocumented failure mode; characterization phase consumes billable slots and invites post-hoc task selection |
| **Payload ≠ test names + assertion output** ([D2]) | Adding prior diff (b) grows the length confound; adding reviewer verdict classes (c) imports the unresolved P6 scoping question into the treatment arm — could either reinforce the P4-C out-of-scope behavior or surface the defect; direction unknown. Raw-text findings (d) withholds the scoping decision but is a hybrid with less precedent |
| **Different retry-instruction bytes** ([D2a]) | The control text is an identity item; any change after launch invalidates slot equality. No text exists yet — nothing to regress against |
| **In-place continuation instead of fresh slots** ([D3]) | Requires supervisor semantics never exercised (`max_attempts=1` in every prior run); state-carryover confounds; complicates the recovery-success definition; attempt-2-baseline-from-failed-snapshot conflates the fix with the prior attempt's partial work |
| **Raising the 90 s decision timeout too** ([D4]) | Second controlled-variable change; makes P5 differ from r2 in two ways; r1 causality (timeout vs model behavior) was never established — a change made to reduce attrition would itself be an unvalidated binary change |
| **`attempt_timeout` reduced to fit `max_duration=3600`** ([D4]) | Cuts the per-attempt budget below the validated 1800 s envelope; risks truncating legitimate attempts and confounds with r2-era timing |
| **3 pairs/condition** ([D5]) | High risk of an uninformative null — P5's expected effect is noisier than r2's maximal one; n=3 was already flagged exploratory in r2 |
| **≥8–10 pairs/condition** ([D5]) | Materially higher cost with no effect estimate yet; unjustified for an exploratory study |
| **Shared attempt-1 seeds** ([D6]) | Halves slot cost but biases both arms identically from one seed; the default independent-pair design avoids this. Deviation requires pre-registered pairing scheme |
| **Third arm now** ([D7]) | +50% slots (30 total) and analysis complexity before any effect estimate exists. Benefit: answers A1 inside the same study |
| **Telemetry added first** ([D8]) | Full separate pre-registration cycle + binary change; delays P5; one-changed-factor discipline survives either way, so the cost is time, not validity. Deferred track becomes more valuable before P7 |
| **Runner retry/backoff bundled into P5** ([D9]) | Violates one-changed-factor: a runtime change inside P5's identity. r2 recovered slot 1 manually via snapshots, so absence is survivable |

---

## 5. Implementation plan after approval (all steps post-approval; none executed now)

Per decision doc §8 sequencing, one factor at a time, each step frozen before the next:

1. **Author and byte-freeze two prompt artifacts** (pure authoring, no code):
   (a) control retry-instruction text; (b) treatment feedback-addendum
   template — payload subset per [D2], header/formatting fixed, source-record
   digests embedded. Digests become identity items.
2. **Create the dated P5 identity manifest** (r2 pattern, e.g.
   `p5-fb-identity.json`): all methodology §3.1 layers + the two new prompt
   artifacts + per-T-slot source-record bindings (which attempt-1 run seeds
   which attempt-2 slot, with record digests). Re-verify the evaluator
   archive and Baseline v2 freeze at freeze time (both verified clean
   2026-10-14; re-verify again at freeze).
3. **Pre-execution amendment to the pre-registration**, recording: task
   choice + stop rule, payload bytes, control-instruction bytes, retry/snapshot
   protocol, timeout deviations (`max_duration` only), sample size, frozen
   slot order, seeding scheme, telemetry declaration. Frozen before launch;
   no edits afterward.
4. *(Optional, [D9])* Separately pre-registered acceptance-runner
   retry/backoff — never bundled into P5's identity.
5. **User authorization** ([D10]) → supervised execution in frozen slot
   order, fail-closed decision table, no silent retries.

The feedback-addendum builder (code assembling the payload into the
attempt-2 prompt) is implemented only after [D2] is approved — it is the
one implementation prerequisite, deliberately deferred (readiness gate 14).

## 6. Files / code areas expected to change (post-approval implementation scope)

**Docs (this directory):**

- `docs/e2e-evals/preregistrations/2026-10-p5-failure-feedback-loop.md` —
  pre-execution amendment (dated, appended — the frozen body is never edited).
- New: `p5` identity manifest (pattern: `/home/liyufeng/p4b-benchmark/ab-prompt-v1/p4d-r2-ab-identity.json`).
- New: control retry-instruction + feedback-addendum byte files.
- Post-run: `docs/e2e-evals/runs/<date>-p5-…-report.md`.

**Code (minimal; all under `llm-harness-runtime/`):**

- The **feedback-addendum builder** — assembles the frozen template +
  evidence artifacts into the attempt-2 prompt. Expected home: alongside the
  supervisor scripts (`p4d_ab_supervise.py` pattern → a `p5_*_supervise.py`
  or a small builder module it imports). **No runtime/evaluator changes.**
- Supervisor parameters: `max_duration` raise only (manifest-level, not code,
  if the supervisor reads it from config — the r2 identity pinned
  `max_attempts=1 / attempt_timeout=1800 / max_duration=3600` as
  configuration, verified above).
- `tools/eval/acceptance_runner.py` retry/backoff **only if** [D9] approved —
  own pre-registration, own identity, not part of P5.
- Explicitly **out of scope**: `crates/llm-harness-agent-team-studio/`
  (runtime binary — runs from the frozen Baseline v2 bundle), the pinned
  evaluator sources (used from the archive).

## 7. Preflight requirements before the first billable run

Every item verified-or-created before slot 1:

1. **[D1]–[D10] approved** and recorded in the frozen pre-execution amendment.
2. **Prompt artifacts byte-frozen:** control retry instruction + treatment
   feedback addendum, digests in the identity manifest.
3. **Identity manifest created and hash-locked** (r2 pattern): task,
   prompts, template, variant, baseline binary/wrapper/CA, evaluators,
   sandbox, parameters, slot order, seeding bindings; file itself hashed;
   per-slot mechanical equality checks prepared (must survive an
   interruption/continuation seam — r2 pattern).
4. **Evaluator archive re-verified at freeze time** — `diff -r`-equivalent
   check against `/home/liyufeng/p4c-aa/frozen-source/p4c-aa-ready-20261013`
   (verified clean 2026-10-14; repeat at freeze).
5. **Baseline v2 freeze re-verified** (`/home/liyufeng/p4c-baseline-freeze/`,
   `snapshot.sha256` clean; binary/wrapper/CA digests match identity).
6. **b2 signature-replication stop rule armed:** the first C attempt-1 slots
   must reproduce the known signature
   (`test_case_duplicates_inside_one_dict_add` +
   `test_report_total_duplicates`) before any T slot launches; else stop and
   amend.
7. **Decision table + outcome classes** restated in the amendment:
   fail-closed stop on infrastructure failure, no silent retries, slot
   replacement only via a new dated manifest, classes 1/2/3 disjoint.
8. **Snapshot/evidence chain ready:** fresh queue root per slot, per-round
   snapshots with manifests, `events.jsonl` bounds recorded, `not_measured`
   recorded with reasons for tokens/api_cost/human_attention/
   event_stream_integrity.
9. **Limitation language fixed in advance:** budget equality unverified
   (treatment prompt larger by construction); wall-clock is the only
   efficiency signal; exploratory status stated in the report.
10. **Authorization** — separate explicit user approval of billable slots.

## 8. Verification basis for this checklist (performed 2026-10-14)

Re-verified on disk during this review (fresh `sha256sum` runs, not copied
from documents): all Baseline v2 freeze digests, b2 prompt/suite/spec
digests, variant digest, template prompt digests (orchestrator/coder/
reviewer match r2 pins), bwrap 0.11.1, Python 3.14.4, frozen-source archive
manifest 13/13 clean, live-vs-pinned `e2e_queue.py` divergence (expected),
r2 identity manifest contents (parameters, slot order B,A,B,A,B,A), and the
existence of per-slot acceptance records with per-test failure names and
assertion output. Open questions and recommendations are quoted from the
P5 documents, not re-derived.

---

## 9. Bottom line

**Ready for approval: YES** — for the *decision set*, not for execution.

The framework, identity chain, and evidence standards are verified in place;
every remaining gap is an explicit human decision ([D1]–[D10]) plus one
deferred implementation task (the feedback-addendum builder, blocked by
design until [D2] is approved). No execution-blocking gap exists.

**Remaining blockers (to execution, in order):**

1. Decisions [D1]–[D9] approved and recorded in the dated pre-execution amendment.
2. Control retry-instruction and feedback-addendum bytes authored and frozen ([D2]/[D2a]).
3. New P5 identity manifest created, hash-locked, preflight-verified (§7 items 3–5).
4. Pre-execution amendment frozen (never edited after launch).
5. Explicit user authorization for billable slots ([D10]) — final gate.
