#!/usr/bin/env python3
"""P4-D b2 prompt A/B launch supervisor (fail-closed six-attempt launcher).

Minimal adaptation of the proven P4-C supervisor (p4c_aa_supervise.py) for
the pre-registered P4-D b2 prompt A/B experiment (ab-prompt-v1). The P4-C
runner, the archived frozen source, and all historical evidence are NOT
modified. This adapter:

1. Accepts the pre-registered six-slot b2-only order with per-slot
   conditions B, A, B, A, B, A (identity file's ``slot_conditions``),
   instead of the P4-C hard-coded b2,b3 interleaving.
2. Enforces condition-specific identity: before EVERY slot the full pinned
   identity of that slot's condition is verified (prompt bytes, acceptance
   spec bytes, definition bytes) plus every shared frozen constant
   (fixture tree, baseline, suite, variant, template, evaluator digests,
   frozen wrapper/binary/CA bundle, preflight pins). The prompt/spec/
   definition difference between A and B is the ONLY declared delta; any
   other drift stops the experiment. At launch both conditions are verified.
3. Enforces per-condition cross-slot configuration equality: the three A
   slots must share one effective configuration identity, the three B slots
   another; A-vs-B differences caused by the declared prompt factor are
   legal. Within a condition any drift stops the run (mirrors the P4-C
   cross-slot equality check).
4. Uses the FROZEN evaluator sources (default: the P4-C frozen-source
   archive) for queue invocation and verification pins — the live
   tools/eval/e2e_queue.py has drifted from the frozen digest 111a494a… and
   MUST NOT be substituted.

Fail-closed behavior is inherited unchanged from the base supervisor:
identity verification before every slot, stop on infrastructure failure /
corrupted evidence / unexpected identity drift / ambiguous outcomes,
measured code_correctness=false is valid data (continue), never silently
retry or replace a slot, fresh queue root per attempt, decision-table-driven
stop/continue, full supervisor JSONL decision log.

No model calls; dry-run verifies identity and plans the order only.
"""

from __future__ import annotations

import argparse
import json
import os
import sqlite3
import subprocess
import sys
from pathlib import Path
from typing import Any

import p4c_aa_supervise as base

# The frozen evaluator tree backing the pinned evaluator digests. The queue
# subprocess (base.run_queue_command → base.E2E_QUEUE) and identity
# verification (base.EVAL_DIR) both resolve through the base module, so the
# adapter repoints them before any verification or invocation.
DEFAULT_FROZEN_EVAL_DIR = Path(
    "/home/liyufeng/p4c-aa/frozen-source/p4c-aa-ready-20261013/tools/eval")

DEFAULT_IDENTITY = Path(
    "/home/liyufeng/p4b-benchmark/ab-prompt-v1/p4d-ab-identity.json")


def use_frozen_evaluators(frozen_eval_dir: Path) -> None:
    """Point the base supervisor's evaluator paths at the frozen copies."""
    global _FROZEN_EVAL_DIR
    if not frozen_eval_dir.is_dir():
        raise base.IdentityError(
            f"frozen evaluator dir missing: {frozen_eval_dir}")
    _FROZEN_EVAL_DIR = frozen_eval_dir
    base.EVAL_DIR = frozen_eval_dir
    base.E2E_QUEUE = frozen_eval_dir / "e2e_queue.py"


_FROZEN_EVAL_DIR = base.EVAL_DIR


class ABIdentity(base.Identity):
    """P4-D A/B identity: per-condition b2 pins + shared frozen constants.

    The identity file keeps the P4-C supervisor schema (so the inherited
    ``verify`` applies unchanged) with two A/B extensions:
      - ``slot_conditions``: ["B","A","B","A","B","A"], aligned with ``order``
        (six b2-inventory-merge entries).
      - ``prepared`` keyed by condition ("A_original"/"B_clarified") instead
        of task id; each entry pins that condition's definition, prompt, and
        acceptance spec using the base digest field names, plus the shared
        constants. Path resolution mirrors the A/B manifest: absolute
        as-is, ``ab-prompt-v1/…`` relative to the ab-prompt-v1 root
        (declared-B paths), everything else relative to ``benchmark_root``.

    ``prepared`` (base-compatible, keyed by composite
    "<task_id>#<condition>") is narrowed to the active condition via
    :meth:`select`, so every inherited check (definition-internal pins,
    fixture/prompt/spec digests, variant, freeze, evaluators, template,
    preflight) runs unchanged against exactly the current condition's
    artifacts — including the A/B declared deltas and nothing else.
    """

    TASK_ID = "b2-inventory-merge"
    CONDITION_KEYS = {"A": "A_original", "B": "B_clarified"}

    def __init__(self, path: Path):
        raw = json.loads(path.read_text())
        self.path = path
        self.raw = raw
        self.benchmark_root = Path(raw["benchmark_root"])

        order = raw["order"]
        conditions = raw.get("slot_conditions")
        expected_conditions = ["B", "A", "B", "A", "B", "A"]
        if not isinstance(order, list) or len(order) != 6:
            raise base.IdentityError(f"order must hold six slots: {order!r}")
        if any(task != self.TASK_ID for task in order):
            raise base.IdentityError(f"A/B order must be b2-only: {order!r}")
        if conditions != expected_conditions:
            raise base.IdentityError(
                f"slot_conditions {conditions!r} != pre-registered "
                f"B,A,B,A,B,A")
        self.slot_conditions = list(conditions)
        # Composite order: "<task_id>#<condition>" per slot.
        self.order = [f"{task}#{cond}"
                      for task, cond in zip(order, conditions)]

        ab_root = path.parent  # ab-prompt-v1/
        prepared: dict[str, dict[str, Any]] = {}
        for cond, key in self.CONDITION_KEYS.items():
            pin = raw["prepared"][key]

            def resolve(declared: str) -> Path:
                p = Path(pin[declared])
                if p.is_absolute():
                    return p
                value = pin[declared]
                if value.startswith("ab-prompt-v1/"):
                    return ab_root / value[len("ab-prompt-v1/"):]
                return self.benchmark_root / value

            entry = {
                "definition": str(resolve("definition")),
                "definition_sha256": pin["definition_sha256"],
                "task_id": self.TASK_ID,
                "version": str(pin["version"]),
                "fixture_dir": str(self.benchmark_root / pin["fixture_dir"]),
                "fixture_tree_digest": pin["fixture_tree_digest"],
                "baseline_digest": pin["baseline_digest"],
                "prompt_path": str(resolve("prompt_path")),
                "prompt_digest": pin.get("prompt_digest",
                                         pin.get("prompt_sha256")),
                "acceptance_spec": str(resolve("acceptance_spec")),
                "acceptance_spec_digest": pin.get(
                    "acceptance_spec_digest", pin.get("acceptance_spec_sha256")),
                "suite_digest": pin["suite_digest"],
                "suite_version": str(pin["suite_version"]),
                "variant_id": pin["variant_id"],
                "variant_digest": pin["variant_digest"],
                "template_digest": pin["template_digest"],
            }
            # Base-compatible resolved paths (base verify() reads these).
            entry["_definition"] = Path(entry["definition"])
            entry["_fixture_dir"] = Path(entry["fixture_dir"])
            entry["_prompt_path"] = Path(entry["prompt_path"])
            entry["_acceptance_spec"] = Path(entry["acceptance_spec"])
            prepared[f"{self.TASK_ID}#{cond}"] = entry
        self._prepared_by_condition = prepared
        # Active pin set: both conditions until a slot narrows it.
        self.prepared = dict(prepared)

        self.evaluators = {name: base.EVAL_DIR / name
                           for name in raw["evaluators"]}
        self.evaluator_pins = raw["evaluators"]

        declared_template = raw["template"]["dir"]
        if declared_template.startswith("llm-harness-runtime:"):
            self.template_dir = Path(base.REPO) / declared_template.split(":", 1)[1]
        else:
            self.template_dir = Path(declared_template)
        # Per-file template pins are recomputed from the live tree; the
        # binding constraint is the pinned template tree digest, checked by
        # the inherited verify() against the prepared definitions.
        self.template_pins = {rel: base.sha_file(self.template_dir / rel)
                              for rel in sorted(
                                  p.relative_to(self.template_dir).as_posix()
                                  for p in self.template_dir.rglob("*")
                                  if p.is_file() and not p.is_symlink())}

        self.preflight_path = self.benchmark_root / raw["preflight"]["path"]
        self.preflight_sha = raw["preflight"]["sha256"]

        self.variant_path = Path(raw["variant"]["path"])
        self.variant_pin = raw["variant"]

        baseline = raw["baseline"]
        freeze = Path(baseline["freeze_dir"])
        self.binary_path = freeze / baseline["bundled_binary"]["path"]
        self.binary_sha = baseline["bundled_binary"]["sha256"]
        self.wrapper_path = freeze / baseline["wrapper"]["path"]
        self.wrapper_sha = baseline["wrapper"]["sha256"]
        self.ca_path = freeze / baseline["ca_bundle"]["path"]
        self.ca_sha = baseline["ca_bundle"]["sha256"]

        self.parameters = raw["parameters"]

    def select(self, condition: str) -> None:
        """Activate exactly one condition's pins for per-slot verification."""
        if condition not in self.CONDITION_KEYS:
            raise base.IdentityError(f"unknown condition {condition!r}")
        composite = f"{self.TASK_ID}#{condition}"
        pin = self._prepared_by_condition[composite]
        # Composite key (adapter) + bare task id (base evidence checks and
        # base verify() iterate prepared entries; the alias repeats the same
        # pin so inherited lookups resolve unchanged).
        self.prepared = {composite: pin, self.TASK_ID: pin}
        self._active_composite = composite

    def verify(self, label: str, condition: str | None = None) -> None:
        """Verify identities. No condition (launch): verify BOTH conditions'
        full identities independently. With a condition: verify exactly that
        condition's full pinned identity (declared A/B deltas allowed, all
        shared constants invariant)."""
        if condition is None:
            for cond in ("A", "B"):
                self.select(cond)
                base.Identity.verify(self, f"{label}#condition-{cond}")
            return
        self.select(condition)
        base.Identity.verify(self, label)

    def snapshot(self) -> dict[str, Any]:
        snap = base.Identity.snapshot(self)
        snap["slot_conditions"] = list(self.slot_conditions)
        return snap


def supervise(args: argparse.Namespace) -> int:
    try:
        identity = ABIdentity(Path(args.identity))
    except (base.IdentityError, OSError, ValueError) as exc:
        print(json.dumps({"event": "stop", "slot": None,
                          "classification": "configuration_mismatch",
                          "reason": str(exc)}, ensure_ascii=False,
                         sort_keys=True), flush=True)
        return 3
    log: base.Log | None = None

    def record(event: str, **fields: Any) -> None:
        if log is None:
            print(json.dumps({"event": event, **fields}, ensure_ascii=False,
                             sort_keys=True), flush=True)
        else:
            log.record(event, **fields)

    snap = identity.snapshot()
    record("supervisor_start", experiment=identity.raw.get("experiment"),
           identity=snap, dry_run=args.dry_run)

    try:
        identity.verify("launch")
    except (base.IdentityError, OSError, ValueError) as exc:
        record("stop", slot=None, classification="configuration_mismatch",
               reason=str(exc))
        return 3

    queue_base = Path(args.queue_base).resolve()
    if queue_base.exists():
        record("stop", slot=None, classification="queue_base_exists",
               reason=f"queue base already exists: {queue_base} (refusing reuse)")
        return 3
    if args.dry_run:
        record("dry_run_complete",
               planned_order=[c.split("#")[0] for c in identity.order],
               planned_conditions=[c.split("#")[1] for c in identity.order])
        return 0
    queue_base.mkdir(parents=True, mode=0o700)
    log = base.Log(queue_base / "supervisor-log.jsonl")

    # Per-condition cross-slot configuration references: within a condition
    # the three slots must share effective configuration identity; across
    # conditions the declared prompt delta may legitimately differ.
    seen_condition_config: dict[str, dict[str, str]] = {}
    saved_seen = base._SEEN_SLOT_CONFIG

    for slot_index in range(1, 7):
        composite = identity.order[slot_index - 1]
        task_id, condition = composite.split("#")
        slot_label = f"slot-{slot_index}-{task_id}-{condition}"

        # Identity re-verification before EVERY slot, condition-scoped.
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
                              f"{(start.stderr or start.stdout).strip()[-500:]}")
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
                                   "outcome_category": None, "result_path": None}
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
                   outcome_category=grading.get("outcome_category"),
                   result_path=grading.get("result_path"),
                   attempt_id=row.get("attempt_id"),
                   workflow_id=row.get("workflow_id"))

        if grading.get("result_path") is not None:
            # Per-condition cross-slot equality: swap the base module's
            # reference dict for this condition's reference, run the
            # inherited evidence enforcement, then store the updated ref.
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
                       note="experiment aborted with fewer than six attempted slots; "
                            "all retained evidence preserved under " + str(queue_base))
            return 10

    log.record("pilot_complete", slots=6,
               order=[c.split("#")[0] for c in identity.order],
               conditions=[c.split("#")[1] for c in identity.order],
               note="all six slots produced valid, decision-table-continue outcomes")
    return 0


def main() -> None:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--queue-base", required=True, type=Path,
                        help="fresh directory for the six attempt queue roots")
    parser.add_argument("--key-file", type=Path,
                        default=Path.home() / ".harness/agent-team-studio/apikey",
                        help="provider key file (read at run time; never copied)")
    parser.add_argument("--identity", type=Path, default=DEFAULT_IDENTITY,
                        help="P4-D A/B identity file (p4d-ab-identity.json)")
    parser.add_argument("--frozen-eval-dir", type=Path,
                        default=DEFAULT_FROZEN_EVAL_DIR,
                        help="frozen evaluator tree backing the pinned digests")
    parser.add_argument("--dry-run", action="store_true",
                        help="verify identities and plan only; no queue invocations")
    args = parser.parse_args()
    use_frozen_evaluators(args.frozen_eval_dir)
    sys.exit(supervise(args))


if __name__ == "__main__":
    main()
