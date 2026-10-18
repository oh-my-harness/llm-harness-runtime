#!/usr/bin/env python3
"""P4-D-r2 continuation driver: executes pre-registered slots 2-6 only.

Slot 1 (condition B) was already executed in run
/home/liyufeng/p4d-ab/runs/r2-20261014T090547 (outcome: completed,
classification acceptance_unavailable due to a transient bwrap namespace
failure). Its acceptance measurement was later recovered separately from
preserved snapshots (evaluation/recovery-acceptance-20261014/, 10/10 pass,
measured code_correctness). Slot 1 is NOT re-executed here.

This driver reuses the frozen p4d_ab_supervise / p4c_aa_supervise modules
UNCHANGED (same decision table, same identity verification, same evidence
enforcement, same frozen evaluators) and runs the identical per-slot loop
for slot_index 2..6. Differences from a full re-launch, each forced by the
continuation semantics (no silent slot replacement, no rerun of slot 1):

1. Slot iteration starts at 2 (pre-registered order preserved:
   slot-2=A, slot-3=B, slot-4=A, slot-5=B, slot-6=A).
2. The condition-B cross-slot configuration reference is seeded from
   slot 1's graded attempt metadata (effective_configuration_digest,
   configuration_fingerprint) instead of from a re-run slot.
3. The supervisor JSONL log records a "continuation_start" event naming the
   slot-1 run root and its classification/recovery, so the two evidence
   roots remain separate and auditable.

Nothing else differs. Any infrastructure failure still stops per the
pre-registered decision table; no retries, no slot replacement.
"""

from __future__ import annotations

import argparse
import json
import os
import sqlite3
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import p4c_aa_supervise as base
import p4d_ab_supervise as p4d

# Frozen evaluator tree (same default as p4d_ab_supervise).
DEFAULT_FROZEN_EVAL_DIR = p4d.DEFAULT_FROZEN_EVAL_DIR
DEFAULT_IDENTITY = p4d.DEFAULT_IDENTITY

# Slot 1 evidence from the interrupted r2 run (never modified, never rerun).
SLOT1_RUN_ROOT = Path("/home/liyufeng/p4d-ab/runs/r2-20261014T090547")
SLOT1_ATTEMPT = (SLOT1_RUN_ROOT /
                 "attempt-1-b2-inventory-merge-B/attempts/"
                 "6e0c98b9-bc0d-4724-a6ac-ec0512603636")
SLOT1_CONDITION = "B"
# Seeded per-condition configuration reference from slot 1's graded metadata.
SLOT1_CONFIG_REF = {
    "effective_configuration_digest":
        "a0c1793d28636f050eb14efdc41d3983cffaa15856a7c9bebde852c38ff875f0",
    "configuration_fingerprint":
        "e1138a5b923450b512651d73680024f41af12c32b2dfa4b14f2248015038f844",
}
SLOT1_CLASSIFICATION = "acceptance_unavailable"
SLOT1_RECOVERY = ("evaluation/recovery-acceptance-20261014 "
                  "(acceptance recovered from preserved snapshots: 10/10 pass, "
                  "code_correctness measured=true; kept separate from the "
                  "original failure record)")


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Continue P4-D-r2 with pre-registered slots 2-6 only.")
    parser.add_argument("--queue-base", required=True, type=Path,
                        help="fresh directory for the five attempt queue roots")
    parser.add_argument("--key-file", type=Path,
                        default=Path.home() / ".harness/agent-team-studio/apikey")
    parser.add_argument("--identity", type=Path, default=DEFAULT_IDENTITY)
    parser.add_argument("--frozen-eval-dir", type=Path,
                        default=DEFAULT_FROZEN_EVAL_DIR)
    args = parser.parse_args()

    p4d.use_frozen_evaluators(args.frozen_eval_dir)

    try:
        identity = p4d.ABIdentity(Path(args.identity))
    except (base.IdentityError, OSError, ValueError) as exc:
        print(json.dumps({"event": "stop", "slot": None,
                          "classification": "configuration_mismatch",
                          "reason": str(exc)}, ensure_ascii=False,
                         sort_keys=True), flush=True)
        return 3

    queue_base = Path(args.queue_base).resolve()
    if queue_base.exists():
        print(json.dumps({"event": "stop", "slot": None,
                          "classification": "queue_base_exists",
                          "reason": f"queue base already exists: {queue_base}"},
                         ensure_ascii=False, sort_keys=True), flush=True)
        return 3

    # Launch-time identity verification of BOTH conditions (inherited).
    try:
        identity.verify("continuation-launch")
    except (base.IdentityError, OSError, ValueError) as exc:
        print(json.dumps({"event": "stop", "slot": None,
                          "classification": "configuration_mismatch",
                          "reason": str(exc)}, ensure_ascii=False,
                         sort_keys=True), flush=True)
        return 3

    queue_base.mkdir(parents=True, mode=0o700)
    log = base.Log(queue_base / "supervisor-log.jsonl")
    log.record("continuation_start",
               experiment=identity.raw.get("experiment"),
               identity=identity.snapshot(),
               continuing_from=str(SLOT1_RUN_ROOT),
               slot1_condition=SLOT1_CONDITION,
               slot1_classification=SLOT1_CLASSIFICATION,
               slot1_recovery=SLOT1_RECOVERY,
               note="slots 2-6 only; slot 1 not rerun; identity unchanged")

    # Per-condition cross-slot configuration references. Condition B is
    # seeded from slot 1's graded attempt metadata (verified against
    # supervisor-log runtime_identity at continuation start).
    seen_condition_config: dict[str, dict[str, str]] = {
        SLOT1_CONDITION: dict(SLOT1_CONFIG_REF)}
    saved_seen = base._SEEN_SLOT_CONFIG

    # Verify the seed actually matches slot 1's on-disk evidence before use.
    metadata = json.loads((SLOT1_ATTEMPT / "metadata.json").read_text())
    seeded = {k: metadata.get(k) for k in SLOT1_CONFIG_REF}
    if seeded != SLOT1_CONFIG_REF:
        log.record("stop", slot=None, classification="configuration_mismatch",
                   reason=f"slot-1 config seed mismatch: {seeded}")
        return 3

    # Slots 2..6 of the frozen order (order verified by ABIdentity).
    for slot_index in range(2, 7):
        composite = identity.order[slot_index - 1]
        task_id, condition = composite.split("#")
        slot_label = f"slot-{slot_index}-{task_id}-{condition}"

        try:
            identity.verify(slot_label, condition)
        except (base.IdentityError, OSError) as exc:
            log.record("stop", slot=slot_label,
                       classification="configuration_mismatch", reason=str(exc))
            return 3

        attempt_dir = queue_base / f"attempt-{slot_index}-{task_id}-{condition}"
        if attempt_dir.exists() or attempt_dir.is_symlink():
            log.record("stop", slot=slot_label, classification="queue_base_exists",
                       reason=f"attempt directory already exists: {attempt_dir}")
            return 3

        definition = identity.prepared[identity._active_composite]["_definition"]
        env = dict(os.environ)
        env.setdefault("PYTHONHASHSEED", "0")

        try:
            enqueue = base.run_queue_command(
                ["enqueue", "--queue", str(attempt_dir), str(definition)], env)
        except subprocess.TimeoutExpired:
            log.record("stop", slot=slot_label, classification="supervisor_timeout",
                       reason=f"enqueue subprocess exceeded "
                              f"{base.SUPERVISOR_SUBPROCESS_TIMEOUT}s; "
                              f"queue root preserved for investigation")
            return 4
        if enqueue.returncode != 0:
            log.record("stop", slot=slot_label, classification="enqueue_failed",
                       reason=f"enqueue exit {enqueue.returncode}: "
                              f"{(enqueue.stderr or enqueue.stdout).strip()[-500:]}")
            return 4

        try:
            start = base.run_queue_command(
                ["start",
                 "--queue", str(attempt_dir),
                 "--studio-bin", str(identity.wrapper_path),
                 "--key-file", str(Path(args.key_file)),
                 "--max-attempts", str(identity.parameters["max_attempts"]),
                 "--max-duration", str(identity.parameters["max_duration"]),
                 "--attempt-timeout", str(identity.parameters["attempt_timeout"]),
                 "--allow-model", identity.variant_pin["models"]["main"],
                 "--allow-provider", identity.variant_pin["provider_host"]],
                env)
        except subprocess.TimeoutExpired:
            log.record("stop", slot=slot_label, classification="supervisor_timeout",
                       reason=f"start subprocess exceeded "
                              f"{base.SUPERVISOR_SUBPROCESS_TIMEOUT}s "
                              f"(> attempt_timeout); queue root sealed, no resubmission")
            return 5
        if start.returncode != 0:
            log.record("stop", slot=slot_label, classification="queue_start_failed",
                       reason=f"start exit {start.returncode}: "
                              f"{(start.stderr or enqueue.stdout).strip()[-500:]}")
            return 5

        db = attempt_dir / "queue.sqlite3"
        if not db.is_file():
            log.record("stop", slot=slot_label, classification="evidence_missing",
                       reason="queue database missing after successful start")
            return 5
        with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as conn:
            conn.row_factory = sqlite3.Row
            rows = [dict(r) for r in conn.execute(
                "SELECT * FROM tasks ORDER BY rowid")]
        if len(rows) != 1:
            log.record("stop", slot=slot_label, classification="evidence_ambiguous",
                       reason=f"expected exactly one queue row, found {len(rows)}")
            return 5
        row = rows[0]
        state, classification = row["state"], row["classification"]
        try:
            attempt_root, run_dir = base.resolve_attempt_root(attempt_dir, row)
        except ValueError as exc:
            log.record("stop", slot=slot_label, classification="evidence_corrupt",
                       reason=f"attempt identity inconsistent: {exc}")
            return 10
        grading: dict[str, Any] = {"result_paths": [], "code_correctness": None,
                                   "outcome_category": None, "result_path": None,
                                   "code_status": None}
        try:
            if run_dir is not None:
                grading = base.read_grading(run_dir)
                if not grading["result_path"]:
                    raise ValueError("graded-run pointer has no readable result package")
            else:
                grading = base.read_grading(attempt_root)
        except (OSError, ValueError) as exc:
            log.record("stop", slot=slot_label, classification="evidence_corrupt",
                       reason=f"graded evidence unreadable or ambiguous: {exc}")
            return 10
        log.record("attempt_outcome", slot=slot_label, condition=condition,
                   state=state, classification=classification,
                   code_correctness=grading.get("code_correctness"),
                   code_status=grading.get("code_status"),
                   outcome_category=grading.get("outcome_category"),
                   result_path=grading.get("result_path"),
                   attempt_id=row.get("attempt_id"),
                   workflow_id=row.get("workflow_id"))

        if grading.get("result_path") is not None:
            base._SEEN_SLOT_CONFIG.clear()
            if condition in seen_condition_config:
                base._SEEN_SLOT_CONFIG.update(
                    {"ref": seen_condition_config[condition]})
            metadata, stop_code = base.require_identity_evidence(
                identity, attempt_root, row, task_id, log, slot_label)
            if metadata is None:
                return stop_code
            seen_condition_config[condition] = dict(
                base._SEEN_SLOT_CONFIG["ref"])
        elif state == "completed":
            log.record("stop", slot=slot_label, classification="evidence_corrupt",
                       reason="completed measurement lacks readable attempt "
                              "metadata or effective team evidence")
            return 10

        if state == "completed":
            if grading.get("outcome_category") != "completed":
                log.record("stop", slot=slot_label, classification="evidence_missing",
                           reason=f"state completed without a valid graded result "
                                  f"package (category={grading.get('outcome_category')!r})",
                           completed_slots=slot_index,
                           note="all retained evidence preserved under " + str(queue_base))
                return 10
            if grading.get("code_status") != "measured":
                log.record("stop", slot=slot_label, classification="evidence_missing",
                           reason=f"completed outcome lacks measured code_correctness "
                                  f"(status={grading.get('code_status')!r})",
                           completed_slots=slot_index,
                           note="all retained evidence preserved under " + str(queue_base))
                return 10
            try:
                result = json.loads(
                    (attempt_root / grading["result_path"]).read_text())
                category, code_status, code_value = base.validate_result(result)
                if category != "completed" or code_status != "measured":
                    raise ValueError("validated result disagrees with scanned evidence")
            except (OSError, ValueError) as exc:
                log.record("stop", slot=slot_label, classification="evidence_corrupt",
                           reason=f"malformed grading result: {exc}",
                           completed_slots=slot_index,
                           note="all retained evidence preserved under " + str(queue_base))
                return 10
            try:
                base.check_acceptance_attribution(run_dir or attempt_root)
            except ValueError as exc:
                log.record("stop", slot=slot_label, classification="evidence_corrupt",
                           reason=str(exc),
                           completed_slots=slot_index,
                           note="all retained evidence preserved under " + str(queue_base))
                return 10
        decision = base.evaluate_outcome(state, classification,
                                         grading.get("code_correctness"))
        log.record("decision", slot=slot_label, verdict=decision["verdict"],
                   reason=decision["reason"],
                   code_correctness=decision.get("code_correctness"))
        if decision["verdict"] == "stop":
            log.record("stop", slot=slot_label, classification=classification,
                       reason=decision["reason"],
                       completed_slots=slot_index,
                       note="continuation aborted with fewer than six attempted "
                            "slots; all retained evidence preserved under "
                            + str(queue_base))
            return 10

    log.record("pilot_complete", slots=6,
               order=[c.split("#")[0] for c in identity.order],
               conditions=[c.split("#")[1] for c in identity.order],
               note="slots 2-6 produced valid, decision-table-continue outcomes; "
                    "slot 1 completed earlier with recovered acceptance evidence")
    return 0


if __name__ == "__main__":
    sys.exit(main())
