# P4-D-r2 — b2 Prompt A/B Completed: Clarified Prompt 3/3 vs Original 0/3 (H1 Supported, Not Proven)

- **Experiment ID:** `p4d-r2-b2-prompt-ab` (P4-D-r2)
- **Date completed:** 2026-10-14
- **Status:** **COMPLETED — 6/6 slots valid functional samples.** Slot 1
  originally stopped on an infrastructure-class acceptance failure
  (`acceptance_unavailable`); the acceptance measurement was later recovered
  from preserved snapshots without re-executing any agent workflow. No slot
  was retried, replaced, or relabeled.
- **Run roots (raw evidence, do not modify):**
  - Slot 1 (condition B): `/home/liyufeng/p4d-ab/runs/r2-20261014T090547/`
  - Slots 2–6 (continuation): `/home/liyufeng/p4d-ab/runs/r2-20261014T090547-continuation/`
  - Slot 1 recovery package (separate from the original failure record):
    `/home/liyufeng/p4d-ab/runs/r2-20261014T090547/attempt-1-b2-inventory-merge-B/attempts/6e0c98b9-bc0d-4724-a6ac-ec0512603636/evaluation/recovery-acceptance-20261014/`
- **Pre-registration:** `ab-prompt-v1/PRE-REGISTRATION-r2.md` (frozen before
  launch; hypothesis, conditions, order, and decision table unchanged during
  execution).

## 1. Research question

Does prompt clarification improve `b2-inventory-merge` success?

Under the adjudicated b2 specification, P4-C/P4-D-r1 b2 failures were
attributed (H1) to the operator prompt scoping the additive fix to
incoming-vs-current only. Condition B adds one sentence explicitly requiring
additive handling of case-equivalent SKU duplicates within `current`, within
`incoming`, and across both. H1 predicts B closes the two failing acceptance
tests (`test_case_duplicates_inside_one_dict_add`,
`test_report_total_duplicates`).

**Falsification criterion (pre-registered):** B does not improve measured
`code_correctness` over A (e.g. B ≤ A). **Support criterion:** B passes
measurably more slots than A; n=3 per condition is exploratory signal only.

## 2. Experimental design

### 2.1 Conditions (exactly one changed factor: operator prompt bytes)

| Condition | Prompt | Digest | Definition digest |
| --- | --- | --- | --- |
| **A — original** | byte-identical to the P4-C pilot b2 prompt | `d014886d…44cb47c` | `099cd84e…bf766` |
| **B — clarified** | adds one sentence covering case-equivalent duplicates within either inventory and across both | `f425eb36…6324c` | `d3794fba…ea40b9` |

Task: `b2-inventory-merge` v2 only; hidden acceptance suite v2 (`a36b4b3e…dd87b`,
10 tests, 20 s timeout); 3 independent repetitions per condition, interleaved
**B, A, B, A, B, A** (frozen order, identical to r1); one attempt per slot,
no retries; fresh queue root per attempt. Pre-registration:
`ab-prompt-v1/PRE-REGISTRATION-r2.md` §1–§2, §6.

### 2.2 Controlled variables (frozen and re-verified before every slot)

| Variable | Pin |
| --- | --- |
| Fixture / git baseline | tree `5cfbe0ae…21077`, baseline digest `0e02ae0e…ffc279` |
| Team template | coding-team v6, tree `c7e40e0c…cbf9a` |
| Variant / model / provider | `p35-glm-5-3-flash-https` (`db90e75a…`), GLM-5.3-Flash via `api.hyper-op.com` (all tiers) |
| Runtime | frozen Baseline v2: binary `aaffd340…0610a`, wrapper `0cb74fb6…784216`, CA `69ff0cf2…bcef4` — including the frozen 90 s Orchestrator decision timeout |
| Sandbox | `bwrap` (bubblewrap 0.11.1) |
| Evaluators | grader `p1.1` `4325acb7…`, runner `p2.5.0` `6c7be50a…`, queue `111a494a…`, collector `44ec16a1…`, variant `596ee3c3…` (frozen-source archive, digests verified at launch and per slot) |
| Supervisor | `p4d_ab_supervise.py` `88eca1c9…b9a4f3` over `p4c_aa_supervise.py` `1f36e88f…75828`; slots 2–6 executed by a continuation driver (`p4d_r2_continue.py`) that reuses the same frozen modules unchanged and seeds the condition-B cross-slot configuration reference from slot 1's graded metadata |
| Parameters | `max_attempts=1`, `attempt_timeout=1800s`, `max_duration=3600s` |
| Configuration fingerprint | `e1138a5b923450b512651d73680024f41af12c32b2dfa4b14f2248015038f844` (identical across all six slots) |

Identity file: `ab-prompt-v1/p4d-r2-ab-identity.json`
(sha256 `9b3cce7e…415a`). Per-condition cross-slot configuration equality was
enforced by the inherited supervisor check: all three A slots share one
effective configuration digest, all three B slots share another, and the only
A-vs-B delta is the declared prompt factor.

## 3. Results

### 3.1 Outcome table (all slots; taxonomy per pre-registration §4)

| Slot | Condition | Queue state | Classification | Acceptance | `code_correctness` | Outcome class | Duration |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | B | completed | completed* | original: infra failure → recovered 10/10 | **true** (recovered) | 1 — functional | 7:06 |
| 2 | A | completed | completed | 8/10 pass, 2 failed | **false** | 1 — functional | 6:10 |
| 3 | B | completed | completed | 10/10 pass | **true** | 1 — functional | 4:36 |
| 4 | A | completed | completed | 8/10 pass, 2 failed | **false** | 1 — functional | 4:33 |
| 5 | B | completed | completed | 10/10 pass | **true** | 1 — functional | 4:23 |
| 6 | A | completed | completed | 8/10 in both r1 and r2, 2 failed | **false** | 1 — functional | 5:42 |

\* Slot 1's queue classification at supervisor stop was
`acceptance_unavailable` (see §4); the completed agent workflow and both
snapshots were valid, and the recovered acceptance measurement yields
measured `code_correctness=true`. The original `result.json` (which records
`not_measured`) is preserved untouched; the recovery package is stored
separately and marked as recovered evidence.

### 3.2 Primary metric: `code_correctness` per condition (class-1 slots only)

| Condition | Pass / total | Pass rate | Acceptance detail |
| --- | --- | --- | --- |
| **A (original)** | **0 / 3** | 0% | 8/10 in every slot; failures always `test_case_duplicates_inside_one_dict_add` + `test_report_total_duplicates` |
| **B (clarified)** | **3 / 3** | 100% | 10/10 in every slot |

### 3.3 Reliability metric (class-1 rate per pre-registration §4)

A = 3/3, B = 3/3 (slot 1 counts once its acceptance measurement is
recovered); overall 6/6 submitted slots reached class 1.

Per-slot evidence packages (each contains `result.json`, acceptance
`r1/r2.json` + logs, snapshot manifests, `report.md`; sha256 digests in each
package):

- Slot 1: `…/r2-20261014T090547/attempt-1-b2-inventory-merge-B/attempts/6e0c98b9-bc0d-4724-a6ac-ec0512603636/evaluation/runs/b4aede2ccb9790d72f118a0a/`
- Slot 2: `…/r2-20261014T090547-continuation/attempt-2-b2-inventory-merge-A/attempts/6266b3a3-f9b1-4249-b11b-b9e0847d97ee/evaluation/runs/0130657057be08e1c5518eac/`
- Slot 3: `…/r2-20261014T090547-continuation/attempt-3-b2-inventory-merge-B/attempts/0af293cb-5202-4eef-8906-fb99b335ba5a/evaluation/runs/37946b62a19ddc210d81fce4/`
- Slot 4: `…/r2-20261014T090547-continuation/attempt-4-b2-inventory-merge-A/attempts/c5749057-4688-4d5f-95df-58ce1d26518d/evaluation/runs/0ec828827773befcf6b87001/`
- Slot 5: `…/r2-20261014T090547-continuation/attempt-5-b2-inventory-merge-B/attempts/d3371999-a148-4b55-abe3-574aafd38e0e/evaluation/runs/d7135ffc9989daa10e87cd80/`
- Slot 6: `…/r2-20261014T090547-continuation/attempt-6-b2-inventory-merge-A/attempts/dad31ea4-625e-427b-8d43-a259ee9fa51b/evaluation/runs/24a3f5e7ccd3a7492e14447b/`

Supervisor decision logs: `…/r2-20261014T090547/supervisor-log.jsonl` and
`…/r2-20261014T090547-continuation/supervisor-log.jsonl` (the latter opens
with a `continuation_start` event binding the two roots). Durations from
`pipeline_started` → `pipeline_completed` in each package's
`source/evidence/events.jsonl`.

## 4. Infrastructure incident: slot-1 bwrap EAGAIN (transient, recovered)

**Facts (verified from evidence):**

- At 2026-10-14 09:12:55 +0800 (after `pipeline_completed` at 09:12:54.98Z),
  the frozen acceptance runner (p2.5.0) invoked bwrap for rounds r1 and r2;
  both failed instantly (exit 1, 0.018 s / 0.009 s, no unittest run):
  `bwrap: Creating new namespace failed: Resource temporarily unavailable`
  (`…/runs/b4aede2ccb9790d72f118a0a/acceptance/r1.log`, `r2.log`; records
  `r1.json`/`r2.json` with `status=infrastructure_failure`).
- All upstream stages were healthy: agent workflow `completed`
  (`pipeline_started` 01:05:48Z → `pipeline_completed` 01:12:54Z; 6 sealed
  worker/decision runs), snapshots r0–r2 collected with verified integrity,
  collector gaps empty, grader `state=graded`. The grader correctly reported
  `code_correctness.status=not_measured` and the supervisor applied the
  pre-registered decision table (`stop`, `acceptance_unavailable`, exit 10).
- The identical bwrap command succeeds on the host afterward
  (`/proc/sys/user/max_user_namespaces=29521` was never the numeric limit);
  the EAGAIN was a transient host-level namespace-creation failure.

**Recovery (no agent execution, no evaluator changes):**

- Preserved snapshot file trees (`r1/files`, `r2/files`) were re-hashed
  against their manifests (`file_hashes` exact match) and the frozen
  spec/fixture/suite digests re-verified byte-for-byte
  (`cec6df41…`, `f425eb36…`, `a36b4b3e…`).
- The exact command recorded in `r1.json`/`r2.json` was re-executed in the
  same bwrap sandbox shape against the preserved snapshots, twice (r1, r2):
  **10/10 pass, OK, exit 0** in both rounds.
- Recovery records (`r1.json`/`r2.json`/`summary.json`, all marked
  `recovered=true` with pointers to the original failure records) live in
  `…/evaluation/recovery-acceptance-20261014/`. The original failure logs and
  records were never modified (re-verified byte-identical post-recovery).

**Classification:** infrastructure failure (host, transient); **not** a
functional failure of condition B and **not** a pipeline logic failure — the
pipeline behaved exactly as pre-registered (fail-closed, preserve, stop).

## 5. Interpretation

**H1 is supported, not proven.**

- The pre-registered falsification criterion (B ≤ A) did not occur. The
  observed separation is the maximum possible at this sample size: B 3/3 vs
  A 0/3.
- Mechanism is consistent with H1: every A slot failed exactly the two
  case-equivalent-duplicate tests the clarified prompt targets, while every
  B slot passed all ten. The failure mode replicated deterministically
  across three independent A runs (including slot 6's extra review round,
  which did not change the 8/10 outcome), so the A result is not a one-off.
- **Limitation:** n=3 per condition. This is exploratory signal under the
  pre-registration's own rule; a reliable claim of prompt-clarification
  benefit requires a larger pre-registered study.
- **Scope:** this speaks only to whether the clarified operator prompt lets
  this team template, this runtime, and this model close these two b2
  acceptance tests. It does **not** establish any general capability
  difference of GLM-5.3-Flash, nor that clarification helps tasks generally.
  Metrics gaps carried over from P4-C/r1 (unmeasured `api_cost`, `tokens`,
  `human_attention`, `event_stream_integrity`) remain unmeasured here.
- Slot-1's pass rests on recovered acceptance evidence (separate artifact
  chain), not on a fresh grading pass; both B-pass and A-fail conclusions
  are unchanged if slot 1 is excluded (B 2/2, A 0/3).

## 6. Lessons for Agentic Coding Team evaluation

1. **Separation of execution evidence and measurement evidence paid for
   itself.** The slot-1 agent workflow (chat transcripts, snapshots, diffs)
   and its acceptance measurement were independent artifact chains. When the
   measurement layer failed (bwrap EAGAIN), the execution evidence remained
   intact and the measurement could be regenerated without re-running — or
   re-paying — the agent workflow. Design evaluations so that every snapshot
   is a self-sufficient, hash-pinned measurement input.
2. **Controlled intervention evaluation requires fail-closed taxonomy
   discipline.** Because the pre-registration defined disjoint outcome
   classes and a no-silent-retry rule, an infrastructure failure could not
   silently become a functional pass/fail or a relabeled rerun; recovery
   happened in a separate, provenance-marked artifact chain and the original
   failure record was preserved byte-identical. Every `code_correctness`
   value in §3 is attributable to a specific frozen command, suite digest,
   and snapshot manifest digest.
3. **Reproducibility requirements observed in practice.** Frozen digests for
   every layer (prompt, fixture, suite, evaluator, binary, template) were
   re-verified before every slot and after the incident; cross-slot
   configuration equality was mechanically enforced (including across the
   two run roots after the interruption). What made the A/B conclusion
   trustworthy was not the sample size but that identity verification was
   continuous and the one changed factor was the only possible delta.
   For future runs: add a bounded retry/backoff inside the acceptance runner
   for transient namespace-creation failures (a runtime change requiring its
   own pre-registration), and consider measuring pipeline-level telemetry
   that this pilot left unmeasured.

## 7. Evidence index (paths only; raw artifacts not copied here)

- Pre-registration and identity: `/home/liyufeng/p4b-benchmark/ab-prompt-v1/PRE-REGISTRATION-r2.md`, `/home/liyufeng/p4b-benchmark/ab-prompt-v1/p4d-r2-ab-identity.json`
- r1 (historical, not pooled): `runs/2026-10-13-p4d-r1-abort.md`
- Run roots: `/home/liyufeng/p4d-ab/runs/r2-20261014T090547/`, `/home/liyufeng/p4d-ab/runs/r2-20261014T090547-continuation/`
- Slot-1 recovery: `…/r2-20261014T090547/attempt-1-b2-inventory-merge-B/attempts/6e0c98b9-bc0d-4724-a6ac-ec0512603636/evaluation/recovery-acceptance-20261014/summary.json`
- Continuation driver: `llm-harness-runtime/p4d_r2_continue.py` (new file; frozen modules and evaluators unmodified)
- Prior reports preserved: `runs/2026-10-13-p4c-aa-baseline-v2.md`, `runs/2026-10-13-p4d-b2-adjudication-ab.md`, `runs/2026-10-13-p4d-r1-abort.md`
