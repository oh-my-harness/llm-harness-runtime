# P5 Execution Amendment — Failure Feedback Loop (Pre-Execution)

- **Created:** 2026-10-14
- **Status:** EXECUTION AMENDMENT RECORDING APPROVED DECISIONS. **NOT
  EXECUTED.** This document records the user's approval of the P5 decision
  set ([D1]–[D9] of the
  [final approval checklist](../designs/P5-final-approval-checklist.md))
  and freezes the experimental parameters for implementation. It does **not**
  authorize billable slots: execution still requires the final authorization
  gate ([D10], §8 below) after the identity manifest is frozen and preflight
  passes.
- **Amends:** [2026-10-p5-failure-feedback-loop.md](2026-10-p5-failure-feedback-loop.md)
  (pre-registration; frozen body unchanged — this amendment resolves its
  §12 open questions 1–6 per the
  [execution decision](../designs/P5-execution-decision.md) as approved).
- **Labeling:** approved decisions are recorded as **decisions** (user
  authorization of the recommendation); constraints quoted from verified
  evidence are labeled **[V]**.

---

## 1. Approved decisions (user authorization, 2026-10-14)

| # | Decision | Approved value |
| --- | --- | --- |
| D1 | Seed task | **`b2-inventory-merge` v2, original prompt** (`d014886d…`) |
| D2 | Feedback payload | **Failed test names + assertion output only**; reviewer findings and reviewer scope judgments **excluded** |
| D2a | Control retry instruction | Normal retry instruction (no structured failure artifacts); bytes authored and frozen before launch |
| D3 | Attempt structure | Attempt 2 = **fresh slot, fresh runtime instance**, workspace from the same fixture baseline; snapshot-based evidence; independent pairs (no shared seeds) |
| D4 | Runtime parameters | **Frozen Baseline v2 unchanged** (binary/wrapper/CA incl. 90 s decision timeout); **only `max_duration` adjusted** to support the two-attempt flow |
| D5 | Sample size | **5 recovery pairs per condition** (20 slots), exploratory |
| D6 | Seeding | Attempt-2 feedback derives only from the same-study paired attempt-1 graded failure; source-record digests frozen per T slot |
| D7 | Third (length-matched) arm | Not in this study; only via a further dated amendment if the primary result is positive |
| D8 | Telemetry | Token/cost **not measured**; budget equality declared an unverified assumption and restated verbatim in analysis |
| D9 | Acceptance-runner retry/backoff | Not adopted; not bundled into P5 |

## 2. Final experiment parameters

**Identity layers (all [V], re-verified 2026-10-14 against the r2 identity
manifest and on-disk artifacts):**

| Layer | Pin |
| --- | --- |
| Task | `b2-inventory-merge` v2; prompt (fixture.txt) `d014886d…44cb47c`; suite `a36b4b3e…bdd87b`; spec `83cf11f5…20be20` (original-prompt spec); fixture tree `5cfbe0ae…21077`; baseline `0e02ae0e…fc279` |
| Model/provider | `p35-glm-5-3-flash-https` (GLM-5.3-Flash via `api.hyper-op.com`), variant `db90e75a…e1ba` |
| Runtime | Baseline v2: binary `aaffd340…610a`, wrapper `0cb74fb6…4216`, CA `69ff0cf2…bcef4`; 90 s Orchestrator decision timeout included, unchanged |
| Template | coding-team v6 tree `c7e40e0c…bf9a`; per-prompt pins incl. orchestrator `3bcfa638…`, coder `8ec17967…`, reviewer `6e7b5815…` |
| Evaluators | Grader p1.1 `4325acb7…`, runner p2.5.0 `6c7be50a…`, queue `111a494a…`, collector `44ec16a1…`, variant `596ee3c3…` — used from the frozen archive `/home/liyufeng/p4c-aa/frozen-source/p4c-aa-ready-20261013/tools/eval` (manifest re-verified 13/13); the live `tools/eval/e2e_queue.py` has drifted and MUST NOT be used |
| Sandbox | bwrap (bubblewrap 0.11.1); Python 3.14.4 |
| Decision table | Frozen `p4c_aa_supervise.DECISION` table unchanged: fail-closed stop on infrastructure/evidence/ambiguity; no silent retries; no slot replacement (replacement only via a new dated manifest, r1→r2 precedent); outcome classes 1/2/3 disjoint |
| Queue discipline | One task per queue root, fresh queue root per slot, `max_attempts=1` per slot, no resubmission |

**Conditions (the single changed factor is the attempt-2 prompt addendum):**

| Arm | Attempt 1 | Attempt 2 (only on measured `code_correctness=false`) |
| --- | --- | --- |
| **C** | Standard b2-original prompt (`d014886d…`) | Standard prompt + **normal retry instruction** (neutral bytes, no failure evidence) |
| **T** | Standard b2-original prompt (identical bytes to C attempt-1) | Standard prompt + **structured feedback addendum**: failed acceptance test names + assertion output, derived byte-faithfully from the paired attempt-1's `acceptance/rN.json` + `rN.log`. **Excluded:** reviewer findings, reviewer scope judgments (P0/P1/P2 classifications), any authored commentary, any adjudication/report content |

- Attempt-1 configuration is byte-identical between arms; conditions differ
  only in attempt-2 prompt content.
- Feedback payload constraints (inherited from pre-reg §4.2): evidence-derived
  only; every artifact entering a prompt is hash-pinned; payload contains no
  evaluation reports or expected-answer hints beyond the failure evidence.

**Budget:**

- `attempt_timeout = 1800 s` per slot — unchanged.
- `max_duration = 4000 s` — the **only** declared deviation from the r2
  parameter set (`3600`), per the approved decision D4. Note [V]: under the
  approved fresh-slot structure each attempt is its own queue root and its
  own `start` invocation, so a single `max_duration` window never contains
  two attempts; the raised value is declared anyway and is a superset of the
  validated envelope.
- Token/cost budget equality between arms: **unverified assumption**
  (`tokens`, `api_cost` = `not_measured`); the T attempt-2 prompt is longer
  by construction. This sentence must be restated verbatim in the analysis.

**Sample and slot order (frozen; 20 slots):**

Slot naming: `slot-<n>-b2-<C|T>-<a1|a2>p<pair>`. a1 = attempt-1 slot;
a2 = attempt-2 slot, launched only if its paired a1 measured
`code_correctness=false`.

| Slot | Arm/attempt | Seed |
| --- | --- | --- |
| 1 | C-a1-p1 | — |
| 2 | C-a1-p2 | — |
| 3 | C-a2-p1 | slot 1 |
| 4 | C-a2-p2 | slot 2 |
| 5 | T-a1-p1 | — |
| 6 | T-a2-p1 | slot 5 |
| 7 | C-a1-p3 | — |
| 8 | C-a2-p3 | slot 7 |
| 9 | T-a1-p2 | — |
| 10 | T-a2-p2 | slot 9 |
| 11 | C-a1-p3 | — |
| 12 | C-a2-p4 | slot 11 |
| 13 | T-a1-p3 | — |
| 14 | T-a2-p3 | slot 13 |
| 15 | C-a1-p4 | — |
| 16 | C-a2-p5 | slot 15 |
| 17 | T-a1-p4 | — |
| 18 | T-a2-p4 | slot 17 |
| 19 | T-a1-p5 | — |
| 20 | T-a2-p5 | slot 19 |

(Seed column states the a1 slot each a2 slot recovers its failure evidence
from; independent pairs — no seed is shared across arms.)

**Conditional-launch and stop rules (pre-registered):**

1. **Signature-replication stop rule:** slots 1–2 (the first two C attempt-1
   slots) must both reproduce the known failure signature —
   `test_case_duplicates_inside_one_dict_add` +
   `test_report_total_duplicates` failing (8/10) — before **any** T slot
   launches. If either does not, stop, classify, and amend; no T slot runs.
2. An attempt-1 slot that measures `code_correctness=true` is valid
   functional data; its paired attempt-2 slot is **not launched** (no
   recovery opportunity exists). The pair is excluded from the recovery-rate
   denominator and reported. No replacement slots (no silent retries); the
   effective number of recovery pairs may fall below 5 and is reported as
   such.
3. Infrastructure or evidence-class failure at any slot → fail-closed stop
   per the frozen decision table.
4. The 90 s decision timeout stays unchanged; an r1-style fatality is handled
   by the decision table (stop; replacement only via a new dated manifest).

## 3. Identity freeze requirements

Before the first billable slot, a hash-locked identity manifest
(`p5-fb-identity.json`, r2 pattern) must exist and pass mechanical per-slot
verification, covering:

1. All §2 identity layers with SHA-256 digests (values above), re-verified at
   freeze time from the durable archives
   (`/home/liyufeng/p4c-baseline-freeze/`,
   `/home/liyufeng/p4c-aa/frozen-source/p4c-aa-ready-20261013/`) — both
   verified clean 2026-10-14; re-verify at freeze.
2. **Prompt identity items that do not exist yet** (blocking prerequisites):
   - (a) the control retry-instruction bytes (appended to the standard
     prompt), one frozen file for all C-a2 slots;
   - (b) the feedback-addendum template bytes (header, quoting format,
     ordering), frozen before any T slot;
   - (c) per-T-a2-slot composed prompt bytes — built from the template plus
     the paired a1 slot's acceptance records — with the seed record digests
     (`rN.json`, `rN.log`) recorded next to each prompt digest.
   Because the treatment prompt differs per T slot, the manifest is
   **staged and append-only**: a1 and C-a2 entries are frozen at launch;
   each T-a2 slot's entry is appended (with the prior manifest hash chained)
   after its seed completes and before that slot launches. No entry is ever
   edited after its slot launches.
3. **Mechanism constraint [V]:** `e2e_queue.prepare()` requires
   `spec["fixture_digest"] == sha(prompt_bytes)`. Every distinct attempt-2
   prompt (C retry, each T feedback) therefore needs its own acceptance-spec
   copy with `fixture_digest` = digest of the composed prompt bytes and the
   suite byte-identical (`a36b4b3e…` unchanged). This is the exact mechanism
   P4-D-r2 used for the clarified prompt (condition B's spec `cec6df41…`
   vs A's `83cf11f5…`). The per-slot prepared definitions pin
   `prompt_path`/`prompt_digest`/`acceptance_spec`/`acceptance_spec_digest`;
   the suite, fixture, variant, template pins are identical across arms.
4. Per-slot verification (ABIdentity pattern): before every slot, that slot's
   full pinned identity is verified; cross-condition differences are legal
   only where this amendment declares them (the attempt-2 prompt bytes);
   within a condition, attempt-2 prompts are per-slot identity items and are
   excluded from cross-slot equality exactly as the declared factor was in
   r2. The cross-slot effective-configuration comparison
   (`effective_configuration_digest`, `configuration_fingerprint`) excludes
   prompt bytes by construction — verified against
   `require_identity_evidence` — so per-slot prompts do not trip equality
   checks.
5. Snapshot/evidence chain: per-round snapshots with manifests,
   `events.jsonl` with `pipeline_started`/`pipeline_completed`, supervisor
   JSONL decision log, fresh queue root per slot, `not_measured` recorded
   with reasons for `tokens`/`api_cost`/`human_attention`/
   `event_stream_integrity`.

## 4. Known risks (declared; carried into the analysis)

1. **A1 information-volume confound:** T's attempt-2 prompt is longer by
   construction; budget equality unverifiable. Any positive result carries
   this confound until the length-matched arm (D7, conditional) runs.
2. **A3 task-choice confound:** b2's failure is proven information-fixable
   (P4-D-r2 condition B 3/3); a positive result may reflect generic clarity
   rather than feedback. Falsification criteria (pre-reg §10) apply:
   signature-unchanged, displaced failures, task dependence.
3. **90 s decision-timeout attrition:** observed 1/13 slots; 20 slots double
   prior exposure. Identical across arms; does not bias the comparison.
   Expected attrition stated in the analysis.
4. **Control condition novelty:** the two-attempt flow has never executed;
   control behavior is unverified until the first C-a2 slots complete.
5. **Scoping override:** reviewers in T-a2 may again scope the defect out
   (P4-C pattern) — a valid outcome redirecting to P6, not a design flaw.
   Note: with reviewer findings excluded from the payload (D2), this study
   cannot distinguish "feedback insufficient" from "scoping layer overrides";
   that boundary belongs to P6.
6. **Exploratory power:** n≤5 pairs/condition bounds effect size coarsely;
   cannot confirm H-FB.
7. **Contamination/history bias on b2:** highest among task options;
   provider-side exposure unknowable; six prior slots replicated the
   signature identically (absence of evidence, not evidence of absence).
8. **Staged-identity risk:** T-a2 prompts are frozen mid-study (append-only).
   A bug in the staging flow could desynchronize manifest and evidence; the
   hash-chain and per-slot verification are the controls. Any drift stops
   the run.

## 5. Remaining authorization gates

1. Prompt artifact bytes authored, approved, and digest-pinned (control
   retry instruction; feedback-addendum template).
2. `p5-fb-identity.json` created, hash-locked; both frozen archives
   re-verified at freeze time.
3. Preflight (checklist §7) passes, including the dry-run
   (identity verification + slot-order plan, **no model calls**).
4. **Explicit user authorization of billable slots** — final gate; nothing
   in this amendment self-executes.

## 6. Standing rules recap

Pre-registration frozen before launch; deviations require a dated amendment,
never silent edits; outcome classes disjoint; infrastructure failures never
functional data; no pooling across experiments; `not_measured` reported as
`not_measured`; original reports never relabeled.
