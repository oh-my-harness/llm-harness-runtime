"""Non-billable regression tests for the P4-C A/A launch supervisor.

Synthetic only: supervise() runs with a stubbed queue-command runner (fake
enqueue/start) and a synthetic identity file in a temp tree. No models, no
frozen binary execution, no writes to the benchmark or freeze directories.
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

sys.path.insert(0, str(Path(__file__).resolve().parent))

import p4c_aa_supervise as sup


def fake_args(queue_base: Path, identity: Path) -> argparse.Namespace:
    return argparse.Namespace(queue_base=queue_base,
                              key_file=Path("/nonexistent/unused-key"),
                              identity=identity, dry_run=False)


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


class Harness:
    """Creates a synthetic benchmark skeleton + identity and a fake queue."""

    def __init__(self, base: Path):
        self.base = base
        self.benchmark = base / "benchmark"
        (self.benchmark / "prepared-baseline").mkdir(parents=True)
        self.template = base / "template"
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
        self.template_digest = sup.tree_digest(self.template)
        self.freeze = base / "freeze"
        self.freeze.mkdir()
        self.binary = self.freeze / "agent-studio-baseline-v2"
        self.binary.write_bytes(b"frozen-binary")
        self.wrapper = self.freeze / "wrapper.sh"
        self.wrapper.write_text("#!/bin/sh\n")
        self.ca = self.freeze / "ca-bundle.pem"
        self.ca.write_bytes(b"ca")
        self.variant_path = base / "variant-baseline.json"
        self.variant = {"id": "synthetic-variant",
                        "models": {"cheap": "m", "main": "m", "strong": "m"},
                        "base_url": "https://synthetic.invalid",
                        "scout_interval_secs": 3600}
        self.variant_path.write_text(json.dumps(self.variant))
        self.tasks: dict[str, dict] = {}
        for task in ("b2-inventory-merge", "b3-rate-limiter"):
            self.add_task(task)
        self.preflight = {"digests": {}}
        self.write_preflight()
        self.identity_path = base / "p4c-aa-identity.json"
        self.write_identity()

    def add_task(self, task: str) -> dict:
        task_root = self.benchmark / "tasks" / task
        fixture = task_root / "fixture"
        fixture.mkdir(parents=True)
        (fixture / "README.md").write_text(f"fixture {task}")
        prompt = task_root / "prompt.txt"
        prompt.write_text(f"prompt {task}")
        spec = task_root / "spec.json"
        spec.write_text("{}")
        definition_path = self.benchmark / "prepared-baseline" / f"{task}.json"
        self.definition_fields = {
            "task_id": task, "version": "2",
            "fixture_tree_digest": sup.tree_digest(fixture),
            "baseline_digest": "0" * 64,
            "prompt_digest": sup.sha_file(prompt),
            "acceptance_spec_digest": sup.sha_file(spec),
            "variant_digest": sup.sha_file(self.variant_path),
            "template_digest": self.template_digest,
            "suite_digest": "suite", "suite_version": "2",
            "variant_id": "synthetic-variant",
        }
        definition_path.write_text(json.dumps(self.definition_fields))
        self.tasks[task] = {"fixture": fixture, "prompt": prompt, "spec": spec,
                            "definition": definition_path,
                            "prompt_sha": self.definition_fields["prompt_digest"],
                            "spec_sha": self.definition_fields["acceptance_spec_digest"]}
        return {"fixture": fixture, "prompt": prompt, "spec": spec,
                "definition": definition_path}

    def write_preflight(self) -> None:
        (self.benchmark / "preflight-digests.json").write_text(
            json.dumps(self.preflight, sort_keys=True))

    def write_identity(self) -> None:
        prepared = {}
        for task, files in self.tasks.items():
            prepared[task] = {
                "definition": str(files["definition"]),
                "definition_sha256": sup.sha_file(files["definition"]),
                "task_id": task, "version": "2",
                "fixture_dir": str(files["fixture"]),
                "fixture_tree_digest": sup.tree_digest(files["fixture"]),
                "baseline_digest": "0" * 64,
                "prompt_path": str(files["prompt"]),
                "prompt_digest": sup.sha_file(files["prompt"]),
                "acceptance_spec": str(files["spec"]),
                "acceptance_spec_digest": sup.sha_file(files["spec"]),
                "suite_digest": "suite", "suite_version": "2",
                "variant_id": "synthetic-variant",
                "variant_digest": sup.sha_file(self.variant_path),
                "template_digest": self.template_digest,
            }
        identity = {
            "experiment": "synthetic",
            "order": ["b2-inventory-merge", "b3-rate-limiter"] * 3,
            "benchmark_root": str(self.benchmark),
            "prepared": prepared,
            "variant": {"path": str(self.variant_path),
                        "sha256": sup.sha_file(self.variant_path),
                        "id": "synthetic-variant",
                        "base_url": "https://synthetic.invalid",
                        "provider_host": "synthetic.invalid",
                        "models": self.variant["models"],
                        "scout_interval_secs": 3600},
            "baseline": {"freeze_dir": str(self.freeze),
                         "bundled_binary": {"path": self.binary.name,
                                            "sha256": sup.sha_file(self.binary)},
                         "wrapper": {"path": self.wrapper.name,
                                     "sha256": sup.sha_file(self.wrapper)},
                         "ca_bundle": {"path": self.ca.name,
                                       "sha256": sup.sha_file(self.ca)}},
            "evaluators": {name: sup.sha_file(sup.EVAL_DIR / name)
                           for name in ("e2e_queue.py", "acceptance_runner.py",
                                        "offline_grader.py", "studio_collector.py",
                                        "variant.py")},
            "template": {"dir": str(self.template), "note": "",
                         "team_template": "coding-team",
                         "files": {rel: sup.sha_file(self.template / rel)
                                   for rel in self.template_files},
                         "template_source_team_json_has_no_sandbox_mode": True},
            "preflight": {"path": "preflight-digests.json",
                          "sha256": sup.sha_file(
                              self.benchmark / "preflight-digests.json"),
                          "note": ""},
            "admission": {"required_sandbox_mode": "bwrap", "note": ""},
            "parameters": {"max_attempts": 1, "max_duration": 3600,
                           "attempt_timeout": 1800},
            "hosts": {"python": "synthetic", "bubblewrap": "synthetic"},
            "limitations": [],
        }
        self.identity_path.write_text(json.dumps(identity, indent=2))

    def refresh_task(self, task: str, definition: dict | None = None) -> None:
        """Re-write task files and the identity after intentional mutation."""
        files = self.tasks[task]
        if definition is not None:
            files["definition"].write_text(json.dumps(definition))
        self.write_identity()

    def runner(self, behaviors: list[dict]) -> tuple:
        """Fake queue command runner.

        behaviors: list of dicts consumed per invocation (enqueue/start).
        Each dict: {returncode, stdout, stderr} and for start optionally
        {row: {...}, artifact: {...}} describing what the supervisor should
        find in the queue root afterwards. The fake materializes the queue
        root (queue.sqlite3, attempts/<id>/...) before returning.
        """
        calls: list[list[str]] = []
        state = {"index": 0}

        def run(command: list[str]) -> subprocess.CompletedProcess:
            calls.append(list(command))
            spec = behaviors[min(state["index"], len(behaviors) - 1)]
            state["index"] += 1
            queue = Path(command[command.index("--queue") + 1])
            row = spec.get("row")
            if row is not None:
                # Real queue rows carry the enqueued task; the attempt dir name
                # identifies the slot's task (attempt-<n>-<task_id>).
                task_id = queue.name.split("-", 2)[2] if queue.name.startswith("attempt-") else "b2-inventory-merge"
                self.materialize_queue(queue, row, spec.get("attempt_id"), task_id)
            return CompletedProcess(command, spec.get("returncode", 0),
                                    spec.get("stdout", ""), spec.get("stderr", ""))

        return run, calls

    def materialize_queue(self, queue: Path, row: dict, attempt_id: str | None = None,
                          task_id: str = "b2-inventory-merge") -> None:
        """Materialize the queue root exactly as e2e_queue.py would: an
        attempt directory with metadata.json (attempt identity, effective
        member configuration, digests), effective-team.json, one evaluation
        run directory with record.json + result.json, and a queue row whose
        artifact_dir points at the graded-run directory after a terminal
        transition (attempt root while preparing)."""
        attempt_id = attempt_id or "a" * 32
        artifact = queue / "attempts" / attempt_id
        run_dir = artifact / "evaluation" / "runs" / "run-1"
        run_dir.mkdir(parents=True, exist_ok=True)
        (run_dir / "record.json").write_text(json.dumps(
            {"project": "eval-" + attempt_id.replace("-", "")[:24],
             "workflow_id": "wf-1", "state": "graded", "mode": "ordinary",
             "acceptance_dir": "acceptance"}))
        metadata_fields = {"attempt_id": attempt_id,
                           "team_id": "eval-" + attempt_id.replace("-", "")[:24],
                           "task_id": task_id,
                           "baseline_digest": "0" * 64,
                           "prompt_digest": self.tasks[task_id]["prompt_sha"],
                           "acceptance_spec_digest": self.tasks[task_id]["spec_sha"],
                           "suite_digest": "suite", "suite_version": "2",
                           "variant_id": "synthetic-variant",
                           "variant_digest": sup.sha_file(self.variant_path),
                           "template_digest": self.template_digest,
                           "models": {"cheap": "m", "main": "m", "strong": "m"},
                           "runtime_binary_digest": "wrapper-digest",
                           "effective_members": [{"member": "orchestrator", "model": "m",
                                                  "provider": "synthetic.invalid"},
                                                 {"member": "coder", "model": "m",
                                                  "provider": "synthetic.invalid"}],
                           "configuration_fingerprint": "fp-fixed"}
        if not row.get("corrupt_effective_team"):
            team_payload = {"id": "eval-" + attempt_id.replace("-", "")[:24], "name": "team",
                            "repo_path": str(artifact / "workspace"),
                            "sandbox_mode": row.get("sandbox", "bwrap"),
                            "members": [{"id": "orchestrator", "model": "strong", "persona": "p"}]}
            (artifact / "effective-team.json").write_text(json.dumps(team_payload))
            # Mirror e2e_queue.py: effective_team_digest is the digest of the
            # attempt's own effective team (unique team ID included);
            # effective_configuration_digest excludes id/name/repo_path.
            metadata_fields["effective_team_digest"] = sup.sha(sup.canonical(team_payload))
            metadata_fields["effective_configuration_digest"] = sup.sha(sup.canonical(
                {key: value for key, value in team_payload.items()
                 if key not in ("id", "name", "repo_path")}))
        else:
            (artifact / "effective-team.json").write_text("{not json")
            metadata_fields["effective_team_digest"] = "etd-fixed"
            metadata_fields["effective_configuration_digest"] = "ecd-fixed"
        if row.get("metadata") is not None:  # field override (drift tests)
            payload = dict(metadata_fields, **row["metadata"])
            (artifact / "metadata.json").write_text(json.dumps(payload))
        elif not row.get("omit_metadata"):
            payload = dict(metadata_fields)
            payload["configuration_fingerprint"] = row.get("config_fingerprint", "fp-fixed")
            (artifact / "metadata.json").write_text(json.dumps(payload))
        if row.get("grading") is not None:
            gradings = [row["grading"]] if isinstance(row["grading"], dict) else row["grading"]
            for index, grading in enumerate(gradings):
                target = (artifact / "evaluation" / "runs" /
                          (f"run-{index + 1}" if isinstance(row["grading"], list) else "run-1"))
                target.mkdir(parents=True, exist_ok=True)
                (target / "record.json").write_text(json.dumps(
                    {"project": "eval-" + attempt_id.replace("-", "")[:24],
                     "workflow_id": "wf-1", "state": "graded", "mode": "ordinary",
                     "acceptance_dir": "acceptance"}))
                (target / "result.json").write_text(json.dumps(grading))
        terminal = row["state"] in ("completed", "failed", "blocked", "interrupted")
        artifact_dir = (str(artifact / "evaluation" / "runs" / "run-1")
                        if terminal and row.get("grading") is not None
                        else str(artifact))
        if row.get("artifact_dir_override") is not None:
            artifact_dir = str(artifact / row["artifact_dir_override"])
        with sqlite3.connect(queue / "queue.sqlite3") as conn:
            conn.executescript(
                "CREATE TABLE IF NOT EXISTS tasks (task_id TEXT PRIMARY KEY, state TEXT NOT NULL,"
                " definition TEXT NOT NULL, attempt_id TEXT, workflow_id TEXT,"
                " artifact_dir TEXT, classification TEXT, error TEXT,"
                " updated_at TEXT NOT NULL);")
            conn.execute("DELETE FROM tasks")
            conn.execute(
                "INSERT OR REPLACE INTO tasks VALUES (?,?,?,?,?,?,?,?,?)",
                (task_id, row["state"],
                 str(queue / "definition.json"), attempt_id, "wf-1",
                 artifact_dir, row.get("classification"), row.get("error"), "now"))


def make_case(harness: Harness, behaviors: list[dict], tmp: Path) -> tuple[int, list]:
    run, calls = harness.runner(behaviors)
    saved = sup.QUEUE_COMMAND_RUNNER
    sup.QUEUE_COMMAND_RUNNER = run
    try:
        code = sup.supervise(fake_args(tmp / "queue-base", harness.identity_path))
    finally:
        sup.QUEUE_COMMAND_RUNNER = saved
    return code, calls


class SupervisorDecisionTests(unittest.TestCase):
    def setUp(self):
        self.base = Path(tempfile.mkdtemp(prefix="p4c-sup-test-"))
        self.addCleanup(shutil.rmtree, self.base, True)
        self.h = Harness(self.base)

    def start_row(self, state, classification, grading=None, sandbox="bwrap",
                  config_fingerprint=None):
        row = {"state": state, "classification": classification,
               "grading": grading, "sandbox": sandbox}
        if config_fingerprint is not None:
            row["config_fingerprint"] = config_fingerprint
        return row

    def test_01_graded_success_continues(self):
        row = self.start_row("completed", "completed", GRADING_COMPLETED_TRUE)
        code, _ = make_case(self.h,
                            [{"row": {"state": "queued"}}, {"row": row}],
                            self.base / "qb")
        self.assertEqual(code, 0)

    def test_02_graded_functional_failure_continues(self):
        row = self.start_row("completed", "completed", GRADING_COMPLETED_FALSE)
        code, _ = make_case(self.h,
                            [{"row": {"state": "queued"}}, {"row": row}],
                            self.base / "qb")
        self.assertEqual(code, 0)

    def test_03_exit_zero_blocked_stops(self):
        row = self.start_row("blocked", "timed_out")
        code, _ = make_case(self.h,
                            [{"row": {"state": "queued"}}, {"row": row}],
                            self.base / "qb")
        self.assertEqual(code, 10)

    def test_04_confinement_unverified_stops(self):
        row = self.start_row("blocked", "confinement_unverified")
        code, _ = make_case(self.h,
                            [{"row": {"state": "queued"}}, {"row": row}],
                            self.base / "qb")
        self.assertEqual(code, 10)

    def test_05_infrastructure_failure_stops(self):
        row = self.start_row("failed", "infrastructure_failed")
        code, _ = make_case(self.h,
                            [{"row": {"state": "queued"}}, {"row": row}],
                            self.base / "qb")
        self.assertEqual(code, 10)

    def test_06_missing_result_package_stops(self):
        row = self.start_row("completed", "completed", grading=None)
        code, _ = make_case(self.h,
                            [{"row": {"state": "queued"}}, {"row": row}],
                            self.base / "qb")
        self.assertEqual(code, 10)

    def test_07_modified_prepared_definition_stops(self):
        definition = json.loads(self.h.tasks["b2-inventory-merge"]["definition"].read_text())
        definition["prompt_digest"] = "f" * 64
        self.h.refresh_task("b2-inventory-merge", definition)
        code, _ = make_case(self.h, [{"row": {"state": "queued"}}], self.base / "qb")
        self.assertEqual(code, 3)

    def test_08_changed_benchmark_input_stops(self):
        self.h.tasks["b3-rate-limiter"]["prompt"].write_text("tampered")
        self.h.refresh_task("b3-rate-limiter")
        code, _ = make_case(self.h, [{"row": {"state": "queued"}}], self.base / "qb")
        self.assertEqual(code, 3)

    def test_09_changed_wrapper_or_binary_stops(self):
        # mutate the wrapper AFTER the identity pinned the frozen digest
        self.h.wrapper.write_text("#!/bin/sh\nexit 1\n")
        code, _ = make_case(self.h, [{"row": {"state": "queued"}}], self.base / "qb")
        self.assertEqual(code, 3)

    def test_10_existing_queue_root_refused(self):
        (self.base / "qb" / "queue-base").mkdir(parents=True)
        code, calls = make_case(self.h, [{"row": {"state": "queued"}}], self.base / "qb")
        self.assertEqual(code, 3)
        self.assertEqual(calls, [])

    def test_11_interrupted_attempt_not_resubmitted(self):
        row = self.start_row("blocked", "submission_uncertain")
        code, calls = make_case(self.h,
                                [{"row": {"state": "queued"}}, {"row": row},
                                 {"row": {"state": "queued"}}],
                                self.base / "qb")
        self.assertEqual(code, 10)
        starts = [c for c in calls if len(c) > 2 and "start" in c]
        self.assertEqual(len(starts), 1)

    def test_12_six_valid_attempts_exact_order(self):
        success = self.start_row("completed", "completed", GRADING_COMPLETED_TRUE)
        behaviors = [{"row": {"state": "queued"}}] + \
                    [{"row": success}] * 6
        code, calls = make_case(self.h, behaviors, self.base / "qb")
        self.assertEqual(code, 0)
        starts = [c for c in calls if len(c) > 2 and "start" in c]
        self.assertEqual(len(starts), 6)
        expected = ["b2-inventory-merge", "b3-rate-limiter"] * 3
        observed = [Path(c[c.index("--queue") + 1]).name
                    for c in starts]
        self.assertEqual(observed,
                         [f"attempt-{i + 1}-{t}" for i, t in enumerate(expected)])

    def test_13_effective_model_drift_stops(self):
        harness = Harness(self.base / "drift")

        def run_with_drift(command: list[str]) -> subprocess.CompletedProcess:
            queue = Path(command[command.index("--queue") + 1])
            if "start" in command:
                row = {"state": "completed", "classification": "completed",
                       "grading": GRADING_COMPLETED_TRUE, "sandbox": "bwrap"}
                harness.materialize_queue(queue, row, "a" * 32, "b2-inventory-merge")
                # Inject effective-model drift into the otherwise valid attempt.
                metadata_path = queue / "attempts" / ("a" * 32) / "metadata.json"
                metadata = json.loads(metadata_path.read_text())
                metadata["models"] = {"cheap": "other", "main": "other", "strong": "other"}
                metadata_path.write_text(json.dumps(metadata))
            return CompletedProcess(command, 0, "", "")

        saved = sup.QUEUE_COMMAND_RUNNER
        sup.QUEUE_COMMAND_RUNNER = run_with_drift
        try:
            code = sup.supervise(fake_args(self.base / "qb-drift", harness.identity_path))
        finally:
            sup.QUEUE_COMMAND_RUNNER = saved
        self.assertEqual(code, 3)

    def test_14_sandbox_mode_drift_stops(self):
        harness = Harness(self.base / "sandbox-drift")
        row = self.start_row("completed", "completed", GRADING_COMPLETED_TRUE,
                             sandbox="os-fallback")
        code, _ = make_case(harness,
                            [{"row": {"state": "queued"}}, {"row": row}],
                            self.base / "qb-sandbox")
        self.assertEqual(code, 3)

    def test_15_unique_team_ids_identical_configuration_complete(self):
        # Regression for D.1: each A/A slot legitimately creates a fresh Team
        # (unique team_id -> unique effective_team_digest); otherwise identical
        # effective configurations must NOT stop the six-slot run.
        success = self.start_row("completed", "completed", GRADING_COMPLETED_TRUE)
        behaviors = [{"row": {"state": "queued"}}] + [{"row": success}] * 6
        code, calls = make_case(self.h, behaviors, self.base / "qb-teamids")
        self.assertEqual(code, 0)
        starts = [c for c in calls if len(c) > 2 and "start" in c]
        self.assertEqual(len(starts), 6)
        # The fake queue derives team IDs from distinct attempt IDs, so the
        # six attempts really carry six different effective_team_digest values.
        digests = set()
        for start in starts:
            queue = Path(start[start.index("--queue") + 1])
            metadata = json.loads((queue / "attempts" / ("a" * 32) /
                                   "metadata.json").read_text())
            digests.add(metadata["effective_team_digest"])
        self.assertEqual(len(digests), 6)
        config_digests = set()
        for start in starts:
            queue = Path(start[start.index("--queue") + 1])
            metadata = json.loads((queue / "attempts" / ("a" * 32) /
                                   "metadata.json").read_text())
            config_digests.add(metadata["effective_configuration_digest"])
        self.assertEqual(len(config_digests), 1)

    def test_16_effective_configuration_digest_drift_stops(self):
        # Cross-attempt equality still blocks genuine configuration changes:
        # one slot whose effective configuration digest differs stops the run.
        success = self.start_row("completed", "completed", GRADING_COMPLETED_TRUE)
        drifted = dict(success)
        drifted["metadata"] = {"effective_configuration_digest": "d" * 64}
        behaviors = [{"row": {"state": "queued"}}, {"row": success},
                     {"row": drifted}]
        code, _ = make_case(self.h, behaviors, self.base / "qb-cfg-drift")
        self.assertEqual(code, 3)

    def test_17_configuration_fingerprint_drift_stops(self):
        # configuration_fingerprint equality is still enforced across slots.
        success = self.start_row("completed", "completed", GRADING_COMPLETED_TRUE,
                                 config_fingerprint="fp-A")
        drifted = self.start_row("completed", "completed", GRADING_COMPLETED_TRUE,
                                 config_fingerprint="fp-B")
        behaviors = [{"row": {"state": "queued"}}, {"row": success},
                     {"row": drifted}]
        code, _ = make_case(self.h, behaviors, self.base / "qb-fp-drift")
        self.assertEqual(code, 3)

    def test_18_effective_team_digest_tamper_stops(self):
        # effective_team_digest stays per-attempt evidence: a digest that does
        # not match the attempt's own effective-team.json stops the experiment.
        success = self.start_row("completed", "completed", GRADING_COMPLETED_TRUE)
        tampered = dict(success)
        tampered["metadata"] = {"effective_team_digest": "0" * 64}
        behaviors = [{"row": {"state": "queued"}}, {"row": tampered}]
        code, _ = make_case(self.h, behaviors, self.base / "qb-etd-tamper")
        self.assertEqual(code, 3)


if __name__ == "__main__":
    unittest.main()
