# P4-D — b2 Specification Adjudication and Prompt A/B Preparation

- **Experiment ID:** `p4d-b2-adjudication-ab`
- **Date:** 2026-10-13
- **Status:** **PREPARED, NOT EXECUTED.** No LLM call, no E2E run, no commit.
  The six P4-C slots remain the official historical record, untouched.
- **Artifacts root:** `/home/liyufeng/p4b-benchmark/ab-prompt-v1/`
  (checksums: `MANIFEST.sha256` there)

## 1. b2 specification adjudication (Task 1)

**Ruling: quantities for case-equivalent SKUs accumulate everywhere — within
`current` alone, within `incoming` alone, and across the two arguments. The
two "failing" acceptance tests are in scope; the benchmark is valid.**

Evidence, by evidence class:

*Explicit public requirements (what the agent is actually shown):*

- Fixture `README.md` (agent-visible, in the workspace): "All keys are SKU
  codes; matching is case-insensitive; merged keys are lowercase; **quantities
  add**." Unqualified — covers all collisions, in any dictionary.
- `inventory.py` docstring (agent-visible, describes the *target* contract):
  "Quantities for the same SKU add up." Not limited to a direction.

*Benchmark designer intent (design-time, not shown to the agent):*

- `task.json` `requirements_covered`: "additive merge **(within each
  dictionary and across both)**" — the designer's own scope statement.
- `validation/b2-inventory-merge-coverage.json` maps
  `test_case_duplicates_inside_one_dict_add` to exactly that requirement, and
  `incorrect/audit-same-dict-overwrite` (adds across dictionaries but
  overwrites in-dict collisions) is shipped as a *declared-incorrect*
  counterexample that the oracle must reject (validation `oracle: pass`,
  3× deterministic). The suite's two "duplicates" tests are the mechanism
  that kills that counterexample.

*Inferred implications:*

- `report.total_units`/`sku_count` derive from `merge_inventories`, so
  `test_report_total_duplicates` follows from the same ruling.
- The reference implementation (sum via `merged.get(key, 0) + quantity` over
  both dicts) implements the broad reading; the acceptance suite matches it
  exactly (`validation/b2-inventory-merge.json`: reference 10/10, start 5/10,
  audit-same-dict-overwrite 8/10, ×3 deterministic).

*Original prompt wording (the narrow text):* `tasks/b2-inventory-merge/
acceptance/fixture.txt` (`d014886d…44cb47c`) specifies only the
incoming-vs-current direction ("when an incoming SKU already exists in
current stock … must ADD"). **Acknowledged:** this narrow wording created a
real scope ambiguity — it made the `current`-internal collision deniable, and
in all three P4-C b2 attempts every Reviewer correctly detected the residual
defect but classified it P2/out-of-scope, and in slot 5 the Orchestrator
explicitly forbade fixing it (chat message id 10: "Explicitly OUT OF SCOPE …
Leave that loop untouched"). **However**, a prompt/README disagreement does
not invalidate the benchmark: the README + docstring are part of the task
materials, unambiguously state the broad contract, and the graded suite is
consistent with both them and the reference solution. The pilot exposed a
prompt-precision weakness, which condition B below addresses directly.

**The recorded P4-C b2 outcomes (3× `code_correctness=false`, 8/10) stand as
graded. No relabeling.**

## 2. A/B conditions (Task 2)

Single changed factor: **operator prompt bytes**. Everything below is held
constant across A and B and pinned by digests in both prepared definitions
(verified identical): fixture tree `5cfbe0ae…`, baseline `0e02ae0e…`,
suite `a36b4b3e…` (v2, 10 tests), suite timeout 20 s, variant
`p35-glm-5-3-flash-https` (`db90e75a…`), template tree `c7e40e0c…`, frozen
runtime binary `aaffd340…` + wrapper `0cb74fb6…` + CA `69ff0cf2…`, evaluator
digests (grader `p1.1`, runner `p2.5.0`, queue `111a494a…`), sandbox `bwrap`,
parameters (max_attempts 1, timeout 1800 s). Grading logic unchanged.

| Condition | Prompt (SHA-256) | Definition (SHA-256) | Acceptance spec (SHA-256) |
| --- | --- | --- | --- |
| **A — original** (byte-identical to pilot) | `d014886dc0b1f4525df9b4fd9a40502360b427bedbe36b1f372af52bb44cb47c` | `prepared/b2-inventory-merge-A-original.json` = `099cd84e…fabf766` (identical to the pilot's pinned definition) | `83cf11f5…62be20` (pinned original) |
| **B — clarified** | `f425eb36fb41fe5770874ee693ce147f61838e726238282cc21826ed42d6324c` | `prepared/b2-inventory-merge-B-clarified.json` = `d3794fba…cea40b9` | `cec6df41…03716a` |

B's prompt = A's wording plus one explicit sentence: quantities accumulate
additively for every case-insensitive SKU collision — within `current` alone,
within `incoming` alone, or across both — before the original
incoming-vs-current sentence (retained verbatim), lowercase-keys and
report.py clauses unchanged.

The `acceptance_spec_digest` differs for B only because `spec.json` pins the
prompt bytes (`fixture_digest`); the suite bytes are identical
(`a36b4b3e…`). Condition A's definition is byte-identical
(`099cd84e…`) to the pilot's, so A is byte-anchored to history while being a
separate artifact — nothing in `prepared-baseline/`, `tasks/`, `prepared/`,
`prepared-v0.2/`, the archives, or the historical run tree was modified
(verified by `preflight-digests.json`-style digest checks below).

Full pin list: `ab-prompt-v1/ab-identity.json` (+ `MANIFEST.sha256`).

## 3. Pre-registered comparison (Task 3)

`ab-prompt-v1/PRE-REGISTRATION.md`, frozen before any run:

- b2 only, 3 repetitions per condition, interleaved **B, A, B, A, B, A**;
  order frozen before launch; one attempt per slot; no silent retries
  (stop-on-invalid per the existing supervisor decision table).
- Code-quality failure (completed, measured false) vs infrastructure failure
  (any other classification) distinguished, as in P4-C.
- Per-attempt result paths and configuration identity preserved per attempt
  (fresh queue root per slot; `metadata.json` digests recorded by the
  unmodified queue).
- Primary metric `code_correctness` (measured) per condition; wall-clock
  duration reported separately. Historical six P4-C runs are **not** pooled.
- **n=3 per condition is exploratory and cannot establish a reliable
  statistical effect**; a directional signal only justifies a larger
  pre-registered study.
- Falsification: B not better than A on completed-slot correctness (details
  in the pre-registration).

## 4. Known measurement gaps (not fixed here)

`api_cost`, `tokens`, `human_attention` (and `event_stream_integrity`)
remain `not_measured` — the Studio-based flow does not capture workflow
usage or interventions. Instrumenting them would change the frozen evaluator
digests mid-series; out of scope for P4-D, recorded as a known gap.

## 5. Real blocker requiring a user decision — RESOLVED (P4-D.1, 2026-10-13)

The pinned `p4c_aa_supervise.py` `Identity` class hard-codes the P4-C order
(`expected == ["b2-inventory-merge", "b3-rate-limiter"] * 3`), so it cannot
supervise a b2-only A/B order unmodified. **Resolution (Option B):** a
minimal, separately reviewed adapter now exists — it does NOT replace the
P4-C runner and does NOT modify it or the archived frozen source.

- **Runner location:** `llm-harness-runtime/p4d_ab_supervise.py`
  (imports `p4c_aa_supervise.py` and reuses its Queue invocation, decision
  table, evidence enforcement, grading validation, and JSONL decision log
  unchanged; only the identity layer and slot loop are adapted).
- **Identity file:** `ab-prompt-v1/p4d-ab-identity.json` — supervisor-schema
  identity twin of `ab-identity.json` with `slot_conditions:
  ["B","A","B","A","B","A"]`; A/B prompt/spec/definition digests per
  condition; all shared constants (fixture/baseline/suite/variant/template/
  evaluator/freeze digests) identical to the P4-C pins.
- **Frozen evaluators:** the adapter resolves evaluator digests and queue
  invocations through the P4-C frozen-source archive
  (`/home/liyufeng/p4c-aa/frozen-source/p4c-aa-ready-20261013/tools/eval`);
  the live `tools/eval/e2e_queue.py` has drifted from the frozen pin
  (`8e2b9a38…` vs `111a494a…`) and is NOT used.
- **Fail-closed behavior preserved:** identity verification before every
  slot (launch verifies A and B independently), predeclared decision table,
  stop on infrastructure failure / corrupt evidence / identity drift /
  ambiguous outcomes, measured `code_correctness=false` is data (continue),
  no silent retries or slot replacement, per-attempt fresh queue roots,
  cross-slot effective-configuration equality **within** each condition
  (the declared prompt delta may legitimately differ across conditions).
- **Offline validation (non-billable, 2026-10-13):**
  `test_p4d_ab_supervise.py` — 22/22 OK (B/A scheduling, wrong-order and
  non-b2 rejection, condition-specific metadata enforcement, legal A/B
  prompt delta, within-condition config drift stop, fixture/suite/variant/
  wrapper/template/model/sandbox drift rejection, correctness true/false
  as data, infrastructure/corrupt/unknown/provider-blocked stop, no
  duplicate submissions after stop); `test_p4c_aa_supervise` 18/18 OK;
  `tools/eval` suite 72/72 OK; `test_p4c_contract_e2e` 7/7 OK;
  `sha256sum -c ab-prompt-v1/MANIFEST.sha256` 7/7 OK.
- **`--dry-run` with the actual P4-D identity: exit 0**; planned order
  b2×6, conditions `B,A,B,A,B,A`; both condition definitions verified
  (`099cd84e…` / `d3794fba…`); evaluator pins resolve to the frozen copies.
- **Remaining limitations:** unchanged from §4 (exploratory n=3; usage
  metrics not captured); additionally, the adapter is validated offline
  with a fake queue only — the first real run is the first live exercise of
  the Queue→Collector→Grader→Supervisor chain with per-condition identity
  (a supervisor stop mid-run preserves all evidence but consumes billable
  slots; per `execution_rules`, a replacement slot needs a new dated
  manifest). ~~`p4d-ab-identity.json` is NOT yet covered by
  `ab-prompt-v1/MANIFEST.sha256`~~ — resolved: the identity file is now
  manifest-covered (see §5a).

## 5a. Provenance finalization (P4-D.1, 2026-10-13, pre-launch)

**Circular-hash audit:** none found. `ab-identity.json` pins the two
definition digests; the definitions contain no identity-file digest (no
self-reference); prompts/specs contain no identity digests;
`p4d-ab-identity.json` contains no self-digest and no runner-script
digest. Hash dependency is a clean DAG: manifest → {definitions,
prompts, specs, suite, identities} → shared constants pinned in the
identity files.

**Provenance digests (recorded; verified 2026-10-13):**

| Artifact | SHA-256 |
| --- | --- |
| `p4d_ab_supervise.py` (P4-D adapter, this experiment's runner) | `88eca1c936d5d1ea5df45bebf54bdcfce7b08f2e1b4563b825b562a2b3b9a4f3` |
| `p4c_aa_supervise.py` (inherited P4-C Supervisor base; identical in live repo and frozen archive `p4c-aa-ready-20261013/`) | `1f36e88f1b5f7edf932cc29053ad52c8ad1c2804962a251967ede16813375828` |
| `p4d-ab-identity.json` (identity twin, now in `MANIFEST.sha256`) | `53a7daf3af3522e3ecb386c0de86f403cbd333e926c7ca3cc4178e8e1ffa99e5` |
| `test_p4d_ab_supervise.py` (offline test suite) | `eafbd38f3cc57af897ef39a922e1b1e8b220597dff2ea69414129916e7d0a672` |
| frozen `e2e_queue.py` (from archive; queue pin `111a494a…`) | `111a494ae2d4b8f71a3aecaf11406fa7df8d17e5334505d35204c3d867264f0e` |
| frozen grader `offline_grader.py` | `4325acb7f74fca5542e4a2f8af16db92ddbcfcfcaad6c25d3d2b63927462e680` |
| frozen `acceptance_runner.py` | `6c7be50aaf4e433980c15d9c011a8c0e454dd858d86fa59e64b759ad4a4246ae` |
| frozen `studio_collector.py` | `44ec16a1dd9bd267a6e509252cee28fd107bf80c3c5edb36acd7fb6f062c4c24` |
| frozen `variant.py` | `596ee3c388794d62b0a3b57627b5e615fa576273dc2943ee0deefda674a95ebf` |

**Manifest verification:** `sha256sum -c ab-prompt-v1/MANIFEST.sha256` —
**8/8 OK** (7 original entries + `p4d-ab-identity.json`; no existing entry
modified, no frozen definition or condition changed).

**Repeated official dry-run (frozen evaluator + actual identity):**
`python3 p4d_ab_supervise.py --queue-base <fresh> --dry-run` → **exit 0**;
planned conditions `B,A,B,A,B,A` over six b2 slots; launch verification of
both conditions passed; evaluator pins resolve to the frozen archive
copies (queue `111a494a…`, grader `4325acb7…`, runner `6c7be50a…`,
collector `44ec16a1…`, variant `596ee3c3…`).

**Precise launch instructions (NOT executed; requires explicit user
authorization; billable):**

```bash
# 0. Pre-flight (non-billable), rerun immediately before launch:
cd /home/liyufeng/p4b-benchmark/ab-prompt-v1 && sha256sum -c MANIFEST.sha256   # 8/8 OK
python3 /home/liyufeng/Projects/llm-harness-runtime/p4d_ab_supervise.py \
  --queue-base <fresh-empty-dir> --identity /home/liyufeng/p4b-benchmark/ab-prompt-v1/p4d-ab-identity.json --dry-run

# 1. Real run (billable; six b2 slots, B,A,B,A,B,A; one attempt per slot;
#    no retries; any stop preserves all evidence under <queue-base>):
python3 /home/liyufeng/Projects/llm-harness-runtime/p4d_ab_supervise.py \
  --queue-base <fresh-empty-dir> \
  --identity /home/liyufeng/p4b-benchmark/ab-prompt-v1/p4d-ab-identity.json \
  --key-file ~/.harness/agent-team-studio/apikey
```

Pin the supervisor scripts for the run (record at authorization time):
`p4d_ab_supervise.py` `88eca1c9…b9a4f3`, inherited `p4c_aa_supervise.py`
`1f36e88f…75828`, identity file `p4d-ab-identity.json`
`53a7daf3…a99e5`. If ANY of these digests differs at launch time, do not
launch.

## 6. Verification performed (non-billable, offline)

- Both A/B definitions produced by the real pinned `e2e_queue.py prepare`
  (which itself validates spec/suite/fixture/variant/template digests).
- `diff` of A vs B definitions: only `prompt_*` and `acceptance_spec*` fields
  differ; `fixture_tree_digest`, `baseline_digest`, `suite_digest`,
  `variant_digest`, `template_digest` byte-equal.
- `sha256sum -c ab-prompt-v1/MANIFEST.sha256`: 7/7 OK.
- Historical pins intact: `tasks/b2-inventory-merge/acceptance/{fixture.txt,
  spec.json}` and `prepared-baseline/b2-inventory-merge.json` unchanged.
- Evaluator suite: `python3 -m unittest discover` in `tools/eval` — 72/72 OK;
  `test_p4c_aa_supervise` — 18/18 OK; `test_p4c_contract_e2e` — 20/20 OK.
- `e2e_queue enqueue` structural dry-check of both definitions into a
  throwaway queue root (staging + digest re-verification, no runtime): pass
  for both A and B.
