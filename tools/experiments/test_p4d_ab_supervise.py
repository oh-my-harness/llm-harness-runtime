"""Non-billable regression tests for the P4-D A/B supervisor adapter.

Synthetic only: supervise() runs with a stubbed queue-command runner (fake
enqueue/start) and a synthetic two-condition identity file in a temp tree.
No models, no frozen binary execution, no writes to the benchmark, ab
directory, freeze directories, or any historical evidence.

Covers (P4-D.1 requirements):
- B/A slot scheduling (pre-registered B,A,B,A,B,A; rejection of A-start and
  of non-b2 orders),
- condition-specific identity matching (A vs B prompts/specs/definitions),
- legal prompt fingerprint differences (declared A/B delta does not stop),
- unexpected prompt/fixture/suite/variant/runtime drift rejection,
- identity mutation between slots,
- valid correctness=true and correctness=false outcomes,
- infrastructure/corrupt/unknown outcome stop behavior,
- no duplicate submissions or silent retries,
- six slots through the real Queue → Collector → Grader → Supervisor
  contract shape via the same fake-queue double used by the P4-C tests.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import unittest


def shutil_rmtree(path: Path) -> None:
    import shutil
    shutil.rmtree(path, True)


sys.path.insert(0, str(Path(__file__).resolve().parent))

import p4c_aa_supervise as base
import p4d_ab_supervise as ab


def fake_args(queue_base: Path, identity: Path) -> argparse.Namespace:
    return argparse.Namespace(queue_base=queue_base,
                              key_file=Path("/nonexistent/unused-key"),
                              identity=identity, dry_run=False,
                              frozen_eval_dir=base.EVAL_DIR)


GRADING_COMPLETED_TRUE = {
    "schema_version": "1", "grader_version": "p1.1",
    "outcome": {"category": "completed", "evidence": [], "uncertainty": None},
    "metrics": {"code_correctness": {"status": "measured", "value": True}},
}
GRADING_COMPLETED_FALSE = {
    "schema_version": "1", "grader_version": "p1.1",
    "outcome": {"category": "completed", "evidence": [], "uncertainty": None},
    "metrics": {"code_correctness": {"status": "measured", "value": False}},
}


class CompletedProcess(subprocess.CompletedProcess):
    def __init__(self, args, returncode, stdout="", stderr=""):
        super().__init__(args, returncode, stdout, stderr)


class ABHarness:
    """Synthetic two-condition benchmark skeleton + A/B identity.

    Mirrors test_p4c_aa_supervise.Harness but with two b2 conditions: A and
    B share all frozen constants (fixture tree, baseline, suite, variant,
    template) and differ only in prompt bytes, acceptance spec bytes (which
    pin the prompt digest), and the definition files.
    """

    def __init__(self, base_dir: Path):
        self.base = base_dir
        self.benchmark = base_dir / "benchmark"
        (self.benchmark / "prepared-ab").mkdir(parents=True)
        self.template = base_dir / "template"
        (self.template / "prompts").mkdir(parents=True)
        self.template_files = {
            "team.json": b"team",
            "prompts/coder.md": b"code",
            "prompts/reviewer.md": b"review",
            "prompts/scout.md": b"scout",
            "prompts/judge.md": b"judge",
            "prompts/orchestrator.md": b"orch",
            "prompts/brainstormer.md": b"brain",
            "prompts/debugger.md": b"debug",
        }
        for rel, data in self.template_files.items():
            path = self.template / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
        self.template_digest = base.tree_digest(self.template)
        self.freeze = base_dir / "freeze"
        self.freeze.mkdir()
        self.binary = self.freeze / "agent-studio-baseline-v2"
        self.binary.write_bytes(b"frozen-binary")
        self.wrapper = self.freeze / "wrapper.sh"
        self.wrapper.write_text("#!/bin/sh\n")
        self.ca = self.freeze / "ca-bundle.pem"
        self.ca.write_bytes(b"ca")
        self.variant_path = base_dir / "variant-baseline.json"
        self.variant = {"id": "synthetic-variant",
                        "models": {"cheap": "m", "main": "m", "strong": "m"},
                        "base_url": "https://synthetic.invalid",
                        "provider_host": "synthetic.invalid",
                        "scout_interval_secs": 3600}
        self.variant_path.write_text(json.dumps(self.variant))

        # Shared b2 fixture tree + suite; per-condition prompt + spec.
        task_root = self.benchmark / "tasks" / "b2-inventory-merge"
        self.fixture = task_root / "fixture"
        self.fixture.mkdir(parents=True)
        (self.fixture / "README.md").write_text("fixture b2")
        self.suite = task_root / "test_acceptance.py"
        self.suite.write_text("suite bytes")
        self.prepared_dir = self.benchmark / "prepared-ab"
        self.conditions: dict[str, dict] = {}
        for cond, prompt_text in (("A", "prompt A"),
                                  ("B", "prompt B with clarification")):
            self.add_condition(cond, prompt_text)
        self.preflight = {"digests": {}}
        self.write_preflight()
        self.identity_path = base_dir / "p4d-ab-identity.json"
        self.write_identity()

    def add_condition(self, cond: str, prompt_text: str) -> dict:
        task = "b2-inventory-merge"
        task_root = self.benchmark / "tasks" / task
        prompt = task_root / f"prompt-{cond}.txt"
        prompt.write_text(prompt_text)
        spec = task_root / f"spec-{cond}.json"
        spec.write_text(json.dumps({"fixture_digest": base.sha_file(prompt)}))
        definition_path = self.benchmark / "prepared-ab" / f"b2-{cond}.json"
        definition_fields = {
            "task_id": task, "version": "2",
            "fixture_tree_digest": base.tree_digest(self.fixture),
            "baseline_digest": "0" * 64,
            "prompt_digest": base.sha_file(prompt),
            "acceptance_spec_digest": base.sha_file(spec),
            "variant_digest": base.sha_file(self.variant_path),
            "template_digest": self.template_digest,
            "suite_digest": "suite", "suite_version": "2",
            "variant_id": "synthetic-variant",
        }
        definition_path.write_text(json.dumps(definition_path and definition_fields))
        self.conditions[cond] = {
            "prompt": prompt, "spec": spec, "definition": definition_path,
            "prompt_sha": definition_fields["prompt_digest"],
            "spec_sha": definition_fields["acceptance_spec_digest"],
            "definition_sha": base.sha_file(definition_path),
        }
        return self.conditions[cond]

    def write_preflight(self) -> None:
        (self.benchmark / "preflight-digests.json").write_text(
            json.dumps(self.preflight, sort_keys=True))

    def write_identity(self,
                       order: list[str] | None = None,
                       slot_conditions: list[str] | None = None) -> None:
        prepared = {}
        for cond, files in self.conditions.items():
            key = {"A": "A_original", "B": "B_clarified"}[cond]
            prepared[key] = {
                "definition": str(files["definition"]),
                "definition_sha256": files["definition_sha"],
                "task_id": "b2-inventory-merge", "version": "2",
                "fixture_dir": str(self.fixture),
                "fixture_tree_digest": base.tree_digest(self.fixture),
                "baseline_digest": "0" * 64,
                "prompt_path": str(files["prompt"]),
                "prompt_sha256": files["prompt_sha"],
                "acceptance_spec": str(files["spec"]),
                "acceptance_spec_sha256": files["spec_sha"],
                "suite_digest": "suite", "suite_version": "2",
                "suite_path": str(self.suite),
                "suite_timeout_seconds": 20,
                "variant_id": "synthetic-variant",
                "variant_digest": base.sha_file(self.variant_path),
                "variant_path": str(self.variant_path),
                "template_digest": self.template_digest,
                "template_dir": str(self.template),
            }
        identity = {
            "experiment": "synthetic-ab",
            "status": "PREPARED_NOT_EXECUTED",
            "order": order or ["b2-inventory-merge"] * 6,
            "slot_conditions": slot_conditions or ["B", "A", "B", "A", "B", "A"],
            "benchmark_root": str(self.benchmark),
            "prepared": prepared,
            "variant": {"path": str(self.variant_path),
                        "sha256": base.sha_file(self.variant_path),
                        "id": "synthetic-variant",
                        "base_url": "https://synthetic.invalid",
                        "provider_host": "synthetic.invalid",
                        "models": self.variant["models"],
                        "scout_interval_secs": 3600},
            "baseline": {"freeze_dir": str(self.freeze),
                         "bundled_binary": {"path": self.binary.name,
                                            "sha256": base.sha_file(self.binary)},
                         "wrapper": {"path": self.wrapper.name,
                                     "sha256": base.sha_file(self.wrapper)},
                         "ca_bundle": {"path": self.ca.name,
                                       "sha256": base.sha_file(self.ca)}},
            "evaluators": {name: base.sha_file(base.EVAL_DIR / name)
                           for name in ("e2e_queue.py", "acceptance_runner.py",
                                        "offline_grader.py", "studio_collector.py",
                                        "variant.py")},
            "template": {"dir": str(self.template), "note": "",
                         "team_template": "coding-team",
                         "files": {rel: base.sha_file(self.template / rel)
                                   for rel in self.template_files}},
            "preflight": {"path": "preflight-digests.json",
                          "sha256": base.sha_file(
                              self.benchmark / "preflight-digests.json"),
                          "note": ""},
            "admission": {"required_sandbox_mode": "bwrap", "note": ""},
            "parameters": {"max_attempts": 1, "max_duration": 3600,
                           "attempt_timeout": 1800},
            "hosts": {"python": "synthetic", "bubblewrap": "synthetic"},
            "limitations": [],
        }
        self.identity_path.write_text(json.dumps(identity, indent=2))

    def refresh(self) -> None:
        """Re-pin the identity after intentional (drift-test) mutation."""
        for cond, files in self.conditions.items():
            files["prompt_sha"] = base.sha_file(files["prompt"])
            files["spec_sha"] = base.sha_file(files["spec"])
            files["definition_sha"] = base.sha_file(files["definition"])
        self.write_identity()

    def runner(self, behaviors: list[dict]) -> tuple:
        """Fake queue command runner (same contract as the P4-C tests)."""
        calls: list[list[str]] = []
        state = {"index": 0}

        def run(command: list[str]) -> subprocess.CompletedProcess:
            calls.append(list(command))
            spec = behaviors[min(state["index"], len(behaviors) - 1)]
            state["index"] += 1
            queue = Path(command[command.index("--queue") + 1])
            row = spec.get("row")
            if row is not None:
                # Slot condition from the attempt dir name
                # attempt-<n>-b2-inventory-merge-<cond>.
                parts = queue.name.split("-")
                condition = parts[-1]
                self.materialize_queue(queue, row, spec.get("attempt_id"),
                                       condition)
            return CompletedProcess(command, spec.get("returncode", 0),
                                    spec.get("stdout", ""), spec.get("stderr", ""))

        return run, calls

    def materialize_queue(self, queue: Path, row: dict, attempt_id: str | None,
                          condition: str) -> None:
        """Materialize one attempt exactly as the P4-C fake queue does, but
        with the CONDITION's prompt/spec digests in metadata.json."""
        attempt_id = attempt_id or "a" * 32
        artifact = queue / "attempts" / attempt_id
        run_dir = artifact / "evaluation" / "runs" / "run-1"
        run_dir.mkdir(parents=True, exist_ok=True)
        (run_dir / "record.json").write_text(json.dumps(
            {"project": "eval-" + attempt_id.replace("-", "")[:24],
             "workflow_id": "wf-1", "state": "graded", "mode": "ordinary",
             "acceptance_dir": "acceptance"}))
        pin = self.conditions[condition]
        metadata_fields = {"attempt_id": attempt_id,
                           "team_id": "eval-" + attempt_id.replace("-", "")[:24],
                           "task_id": "b2-inventory-merge",
                           "baseline_digest": "0" * 64,
                           "prompt_digest": pin["prompt_sha"],
                           "acceptance_spec_digest": pin["spec_sha"],
                           "suite_digest": "suite", "suite_version": "2",
                           "variant_id": "synthetic-variant",
                           "variant_digest": base.sha_file(self.variant_path),
                           "template_digest": self.template_digest,
                           "models": {"cheap": "m", "main": "m", "strong": "m"},
                           "runtime_binary_digest": "wrapper-digest",
                           "effective_members": [
                               {"member": "orchestrator", "model": "m",
                                "provider": "synthetic.invalid"},
                               {"member": "coder", "model": "m",
                                "provider": "synthetic.invalid"}],
                           "configuration_fingerprint": f"fp-{condition}"}
        team_payload = {"id": "eval-" + attempt_id.replace("-", "")[:24],
                        "name": "team",
                        "repo_path": str(artifact / "workspace"),
                        "sandbox_mode": row.get("sandbox", "bwrap"),
                        "members": [{"id": "orchestrator", "model": "strong",
                                     "persona": "p"}]}
        (artifact / "effective-team.json").write_text(
            "{not json" if row.get("corrupt_effective_team")
            else json.dumps(team_payload))
        if row.get("corrupt_effective_team"):
            metadata_fields["effective_team_digest"] = "etd-fixed"
            metadata_fields["effective_configuration_digest"] = "ecd-fixed"
        else:
            metadata_fields["effective_team_digest"] = base.sha(base.canonical(team_payload))
            metadata_fields["effective_configuration_digest"] = base.sha(base.canonical(
                {key: value for key, value in team_payload.items()
                 if key not in ("id", "name", "repo_path")}))
        if row.get("metadata") is not None:
            payload = dict(metadata_fields, **row["metadata"])
            (artifact / "metadata.json").write_text(json.dumps(payload))
        elif not row.get("omit_metadata"):
            payload = dict(metadata_fields)
            payload["configuration_fingerprint"] = row.get(
                "config_fingerprint", metadata_fields["configuration_fingerprint"])
            (artifact / "metadata.json").write_text(json.dumps(payload))
        if row.get("grading") is not None:
            (run_dir / "result.json").write_text(json.dumps(row["grading"]))
        terminal = row["state"] in ("completed", "failed", "blocked", "interrupted")
        artifact_dir = (str(artifact / "evaluation" / "runs" / "run-1")
                        if terminal and row.get("grading") is not None
                        else str(artifact))
        with sqlite3.connect(queue / "queue.sqlite3") as conn:
            conn.executescript(
                "CREATE TABLE IF NOT EXISTS tasks (task_id TEXT PRIMARY KEY, state TEXT NOT NULL,"
                " definition TEXT NOT NULL, attempt_id TEXT, workflow_id TEXT,"
                " artifact_dir TEXT, classification TEXT, error TEXT,"
                " updated_at TEXT NOT NULL);")
            conn.execute("DELETE FROM tasks")
            conn.execute(
                "INSERT OR REPLACE INTO tasks VALUES (?,?,?,?,?,?,?,?,?)",
                ("b2-inventory-merge", row["state"],
                 str(queue / "definition.json"), attempt_id, "wf-1",
                 artifact_dir, row.get("classification"), row.get("error"), "now"))


def make_case(harness: ABHarness, behaviors: list[dict], tmp: Path) -> tuple[int, list]:
    run, calls = harness.runner(behaviors)
    saved = base.QUEUE_COMMAND_RUNNER
    base.QUEUE_COMMAND_RUNNER = run
    try:
        code = ab.supervise(fake_args(tmp / "queue-base", harness.identity_path))
    finally:
        base.QUEUE_COMMAND_RUNNER = saved
    return code, calls


def completed_row(harness: ABHarness, condition: str, grading=None, **row_extra):
    """A start-behavior row whose materialized metadata carries the given
    condition's pinned prompt/spec digests."""
    pin = harness.conditions[condition]
    row = {"state": "completed", "classification": "completed",
           "grading": GRADING_COMPLETED_TRUE if grading is None else grading}
    row.update(row_extra)
    row["_condition"] = condition
    return row


class ABSupervisorTests(unittest.TestCase):
    def setUp(self):
        self.base_dir = Path(tempfile.mkdtemp(prefix="p4d-ab-test-"))
        self.addCleanup(shutil_rmtree, self.base_dir)
        self.h = ABHarness(self.base_dir)

    # -- 1. slot scheduling -------------------------------------------------

    def test_01_six_slots_preregistered_ba_order(self):
        behaviors = [{"row": {"state": "queued"}}] + [
            {"row": {"state": "completed", "classification": "completed",
                     "grading": GRADING_COMPLETED_TRUE, "condition": c}}
            for c in ("B", "A", "B", "A", "B", "A")]
        code, calls = make_case(self.h, behaviors, self.base_dir / "qb")
        self.assertEqual(code, 0)
        starts = [c for c in calls if "start" in c]
        self.assertEqual(len(starts), 6)
        observed = [Path(c[c.index("--queue") + 1]).name for c in starts]
        self.assertEqual(observed, [
            f"attempt-{i + 1}-b2-inventory-merge-{c}"
            for i, c in enumerate(["B", "A", "B", "A", "B", "A"])])

    def test_02_wrong_condition_order_rejected(self):
        # A-start (A,B,A,B,A,B) is NOT the pre-registered order.
        self.h.write_identity(slot_conditions=["A", "B", "A", "B", "A", "B"])
        code, calls = make_case(self.h, [{"row": {"state": "queued"}}],
                                self.base_dir / "qb")
        self.assertEqual(code, 3)
        self.assertEqual(calls, [])

    def test_03_non_b2_order_rejected(self):
        self.h.write_identity(order=["b2-inventory-merge", "b3-rate-limiter"] * 3)
        code, _ = make_case(self.h, [{"row": {"state": "queued"}}],
                            self.base_dir / "qb")
        self.assertEqual(code, 3)

    def test_04_dry_run_plans_without_invocations(self):
        args = fake_args(self.base_dir / "qb", self.h.identity_path)
        args.dry_run = True
        code = ab.supervise(args)
        self.assertEqual(code, 0)

    # -- 2/3. condition-specific identity + legal prompt delta --------------

    def test_05_condition_specific_metadata_enforced(self):
        # Slot 1 (B) whose attempt metadata carries A's prompt/spec digests:
        # identity drift within the slot's own condition → stop.
        success_b = {"row": {"state": "completed", "classification": "completed",
                             "grading": GRADING_COMPLETED_TRUE,
                             "condition": "B",
                             "metadata": {"prompt_digest":
                                          self.h.conditions["A"]["prompt_sha"],
                                          "acceptance_spec_digest":
                                          self.h.conditions["A"]["spec_sha"]}}}
        behaviors = [{"row": {"state": "queued"}}, success_b]
        code, _ = make_case(self.h, behaviors, self.base_dir / "qb")
        self.assertEqual(code, 3)

    def test_06_legal_ab_prompt_delta_continues(self):
        # B slots use B's prompt/spec digests, A slots A's; the differing
        # effective_configuration_digest/config fingerprints across
        # conditions must NOT stop the run; within a condition they repeat.
        behaviors = [{"row": {"state": "queued"}}]
        for cond in ("B", "A", "B", "A", "B", "A"):
            behaviors.append({"row": {
                "state": "completed", "classification": "completed",
                "grading": (GRADING_COMPLETED_TRUE if cond == "A"
                            else GRADING_COMPLETED_FALSE),
                "condition": cond}})
        code, calls = make_case(self.h, behaviors, self.base_dir / "qb")
        self.assertEqual(code, 0)
        self.assertEqual(len([c for c in calls if "start" in c]), 6)

    def test_07_same_condition_config_drift_stops(self):
        # Two A slots must share effective configuration identity.
        behaviors = [
            {"row": {"state": "queued"}},
            {"row": {"state": "completed", "classification": "completed",
                     "grading": GRADING_COMPLETED_TRUE, "condition": "B"}},
            {"row": {"state": "completed", "classification": "completed",
                     "grading": GRADING_COMPLETED_TRUE, "condition": "A"}},
            {"row": {"state": "completed", "classification": "completed",
                     "grading": GRADING_COMPLETED_TRUE, "condition": "B",
                     "metadata": {"effective_configuration_digest": "d" * 64}}},
        ]
        code, _ = make_case(self.h, behaviors, self.base_dir / "qb")
        self.assertEqual(code, 3)

    # -- 4. unexpected drift rejection --------------------------------------

    def test_08_prompt_drift_between_slots_stops(self):
        # Identity mutation between slots: B's prompt file mutates after
        # slot 1; slot 2's identity re-verification must stop the run.
        behaviors = [{"row": {"state": "queued"}},
                     {"row": {"state": "completed", "classification": "completed",
                              "grading": GRADING_COMPLETED_TRUE,
                              "condition": "B"}}]

        original = ab.supervise

        def supervise_with_mutation(args):
            # Mutate after slot-1 enqueue: the loop re-verifies before slot 2.
            self.h.conditions["B"]["prompt"].write_text("tampered between slots")
            self.h.refresh()
            return original(args)

        saved = base.QUEUE_COMMAND_RUNNER
        base.QUEUE_COMMAND_RUNNER = self._mutating_runner(
            behaviors, supervise_with_mutation)
        try:
            code = ab.supervise(fake_args(self.base_dir / "qb",
                                          self.h.identity_path))
        finally:
            base.QUEUE_COMMAND_RUNNER = saved
        self.assertIn(code, (3, 10))

    def _mutating_runner(self, behaviors, hook):
        """Fake runner that triggers `hook` between the first and second start."""
        run, calls = self.h.runner(behaviors)
        state = {"started": 0}

        def run2(command):
            result = run(command)
            if "start" in command:
                state["started"] += 1
                if state["started"] == 1:
                    hook(_fake_args_from(self.h, self.base_dir))
            return result
        return run2

    def test_09_fixture_tree_drift_stops(self):
        (self.h.fixture / "README.md").write_text("tampered fixture")
        code, _ = make_case(self.h, [{"row": {"state": "queued"}}],
                            self.base_dir / "qb")
        self.assertEqual(code, 3)

    def test_10_suite_drift_stops(self):
        # Pin the suite bytes via a preflight pin (the real P4-D identity
        # pins the historical suite through preflight-digests.json), then
        # tamper with the file: the identity re-verification must stop.
        suite_rel = "tasks/b2-inventory-merge/test_acceptance.py"
        self.h.preflight["digests"][suite_rel] = base.sha_file(self.h.suite)
        self.h.write_preflight()
        self.h.write_identity()
        # Sanity: clean state verifies.
        ab.ABIdentity(self.h.identity_path).verify("launch")
        # Tamper with the suite → drift must be rejected.
        self.h.suite.write_text("tampered suite")
        identity = ab.ABIdentity(self.h.identity_path)
        with self.assertRaises(base.IdentityError):
            identity.verify("launch")

    def test_11_variant_drift_stops(self):
        self.h.variant_path.write_text(json.dumps(
            {**self.h.variant, "models": {"cheap": "x", "main": "x",
                                          "strong": "x"}}))
        code, _ = make_case(self.h, [{"row": {"state": "queued"}}],
                            self.base_dir / "qb")
        self.assertEqual(code, 3)

    def test_12_wrapper_drift_stops(self):
        self.h.wrapper.write_text("#!/bin/sh\nexit 1\n")
        code, _ = make_case(self.h, [{"row": {"state": "queued"}}],
                            self.base_dir / "qb")
        self.assertEqual(code, 3)

    def test_13_template_drift_stops(self):
        (self.h.template / "prompts" / "coder.md").write_text("tampered")
        code, _ = make_case(self.h, [{"row": {"state": "queued"}}],
                            self.base_dir / "qb")
        self.assertEqual(code, 3)

    # -- 5. measured outcomes -----------------------------------------------

    def test_14_correctness_true_completes(self):
        behaviors = [{"row": {"state": "queued"}}] + [
            {"row": {"state": "completed", "classification": "completed",
                     "grading": GRADING_COMPLETED_TRUE, "condition": c}}
            for c in ("B", "A", "B", "A", "B", "A")]
        code, _ = make_case(self.h, behaviors, self.base_dir / "qb")
        self.assertEqual(code, 0)

    def test_15_correctness_false_is_data_not_failure(self):
        behaviors = [{"row": {"state": "queued"}}] + [
            {"row": {"state": "completed", "classification": "completed",
                     "grading": GRADING_COMPLETED_FALSE, "condition": c}}
            for c in ("B", "A", "B", "A", "B", "A")]
        code, starts = make_case(self.h, behaviors, self.base_dir / "qb")
        self.assertEqual(code, 0)
        self.assertEqual(len([c for c in starts if "start" in c]), 6)

    # -- 6. stop behavior ----------------------------------------------------

    def test_16_infrastructure_failure_stops(self):
        behaviors = [{"row": {"state": "queued"}},
                     {"row": {"state": "failed",
                              "classification": "infrastructure_failed",
                              "condition": "B"}}]
        code, _ = make_case(self.h, behaviors, self.base_dir / "qb")
        self.assertEqual(code, 10)

    def test_17_corrupt_evidence_stops(self):
        behaviors = [{"row": {"state": "queued"}},
                     {"row": {"state": "completed", "classification": "completed",
                              "grading": GRADING_COMPLETED_TRUE,
                              "condition": "B",
                              "corrupt_effective_team": True}}]
        code, _ = make_case(self.h, behaviors, self.base_dir / "qb")
        self.assertEqual(code, 10)

    def test_18_unknown_outcome_stops(self):
        behaviors = [{"row": {"state": "queued"}},
                     {"row": {"state": "blocked", "classification": "unknown",
                              "condition": "B"}}]
        code, _ = make_case(self.h, behaviors, self.base_dir / "qb")
        self.assertEqual(code, 10)

    def test_19_provider_blocked_stops(self):
        behaviors = [{"row": {"state": "queued"}},
                     {"row": {"state": "blocked",
                              "classification": "provider_blocked",
                              "condition": "B"}}]
        code, _ = make_case(self.h, behaviors, self.base_dir / "qb")
        self.assertEqual(code, 10)

    def test_20_no_duplicate_submission_after_stop(self):
        behaviors = [{"row": {"state": "queued"}},
                     {"row": {"state": "blocked", "classification": "timed_out",
                              "condition": "B"}},
                     {"row": {"state": "queued"}}]
        code, calls = make_case(self.h, behaviors, self.base_dir / "qb")
        self.assertEqual(code, 10)
        starts = [c for c in calls if "start" in c]
        self.assertEqual(len(starts), 1)

    def test_21_sandbox_drift_stops(self):
        behaviors = [{"row": {"state": "queued"}},
                     {"row": {"state": "completed", "classification": "completed",
                              "grading": GRADING_COMPLETED_TRUE, "condition": "B",
                              "sandbox": "os-fallback"}}]
        code, _ = make_case(self.h, behaviors, self.base_dir / "qb")
        self.assertEqual(code, 3)

    def test_22_effective_model_drift_stops(self):
        harness = ABHarness(self.base_dir / "model-drift")

        def run_with_drift(command):
            queue = Path(command[command.index("--queue") + 1])
            if "start" in command:
                row = {"state": "completed", "classification": "completed",
                       "grading": GRADING_COMPLETED_TRUE}
                harness.materialize_queue(queue, row, "a" * 32, "B")
                metadata_path = (queue / "attempts" / ("a" * 32)
                                 / "metadata.json")
                metadata = json.loads(metadata_path.read_text())
                metadata["models"] = {"cheap": "other", "main": "other",
                                      "strong": "other"}
                metadata_path.write_text(json.dumps(metadata))
            return CompletedProcess(command, 0, "", "")

        saved = base.QUEUE_COMMAND_RUNNER
        base.QUEUE_COMMAND_RUNNER = run_with_drift
        try:
            code = ab.supervise(fake_args(self.base_dir / "qb-model-drift",
                                          harness.identity_path))
        finally:
            base.QUEUE_COMMAND_RUNNER = saved
        self.assertEqual(code, 3)


def _fake_args_from(harness: ABHarness, base_dir: Path):
    return fake_args(base_dir / "qb", harness.identity_path)


# The adapter reads args.queue_base / args.identity / args.dry_run only.
# Alias for the mutation test above.
_fake_args = fake_args


if __name__ == "__main__":
    unittest.main()
