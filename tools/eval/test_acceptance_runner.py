"""Synthetic fixture tests for independent acceptance execution and P1 integration."""

import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from acceptance_runner import run_acceptance
from offline_grader import grade


def add_snapshot(root, round_no, content, issue="chat-1"):
    folder = root / "workspace/.pipeline/snapshots/run-1" / f"r{round_no}"
    (folder / "files").mkdir(parents=True)
    rel = "calc.py"
    (folder / "files" / rel).write_bytes(content)
    digest = hashlib.sha256(f"{len(rel)}\x1f{rel}\x1f{len(content)}\x1e".encode() + content).hexdigest()
    obj = {"issue_id": issue, "run_id": "run-1", "round": round_no,
           "changed_files": [rel], "file_hashes": {rel: hashlib.sha256(content).hexdigest()},
           "content_hash": digest, "snapshot_id": f"{issue}#r{round_no}#{digest}"}
    if round_no:
        obj.update(task_start_baseline=str(root / "workspace/.pipeline/snapshots/run-1/r0/snapshot.json"),
                   task_relative_changed_paths=[rel], task_relative_diff="task-relative.patch")
        (folder / "task-relative.patch").write_text("patch")
    (folder / "snapshot.json").write_text(json.dumps(obj))
    return folder


def make_spec(root, test_text):
    fixture = root / "fixture"
    fixture.mkdir()
    suite = fixture / "test_acceptance.py"
    suite.write_text(test_text)
    fixture_file = fixture / "fixture.txt"
    fixture_file.write_text("Implement arithmetic addition.\n")
    spec = fixture / "spec.json"
    spec.write_text(json.dumps({"fixture_id": "arithmetic", "fixture_version": "1",
                                "fixture_path": fixture_file.name,
                                "fixture_digest": hashlib.sha256(fixture_file.read_bytes()).hexdigest(),
                                "timeout_seconds": 2,
                                "suite_version": "1", "suite_path": suite.name,
                                "suite_digest": hashlib.sha256(suite.read_bytes()).hexdigest()}))
    return spec


class AcceptanceRunnerTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.source = self.root / "source"
        self.source.mkdir()
        add_snapshot(self.source, 0, b"def add(a,b): return 0\n")
        self.suite = make_spec(self.root, "import unittest\nfrom calc import add\n"
                               "class TestAdd(unittest.TestCase):\n"
                               " def test_one(self): self.assertEqual(add(1, 2), 3)\n"
                               " def test_two(self): self.assertEqual(add(2, 2), 4)\n")

    def run_one(self, content=b"def add(a,b): return a+b\n", **kwargs):
        add_snapshot(self.source, 1, content)
        return run_acceptance(self.source, self.suite, self.root / "output", **kwargs)

    def test_success_real_bubblewrap_and_grading(self):
        result = self.run_one()
        self.assertEqual(result[0]["status"], "completed")
        self.assertEqual(result[0]["isolation"], "bubblewrap")
        self.assertEqual(result[0]["counts"], {"total": 2, "passed": 2, "failed": 0,
                                                "errors": 0, "skipped": 0})
        scored = grade(self.source, acceptance_dir=self.root / "output")
        self.assertEqual(scored["metrics"]["code_correctness"]["value"], True)
        self.assertEqual(scored["metrics"]["revision_improvement"]["status"], "not_measured")
        self.assertEqual(scored, grade(self.source, acceptance_dir=self.root / "output"))

    def test_incorrect_and_mixed_assertions(self):
        result = self.run_one(b"def add(a,b): return a+b if a==1 else 0\n")
        self.assertEqual(result[0]["counts"]["passed"], 1)
        self.assertEqual(result[0]["counts"]["failed"], 1)
        self.assertFalse(grade(self.source, acceptance_dir=self.root / "output")["metrics"]["code_correctness"]["value"])

    def test_invalid_snapshot_never_runs(self):
        folder = add_snapshot(self.source, 1, b"def add(a,b): return a+b\n")
        (folder / "files/calc.py").write_text("damaged")
        with patch("acceptance_runner.run_process", side_effect=AssertionError("executed")):
            result = run_acceptance(self.source, self.suite, self.root / "output")
        self.assertEqual(result[0]["status"], "verification_failed")

    def test_changed_suite_digest_is_rejected(self):
        add_snapshot(self.source, 1, b"def add(a,b): return a+b\n")
        (self.suite.parent / "test_acceptance.py").write_text("changed")
        with self.assertRaisesRegex(ValueError, "suite digest mismatch"):
            run_acceptance(self.source, self.suite, self.root / "output")
        self.assertFalse((self.root / "output").exists())

    def test_wrong_pinned_spec_digest_is_rejected(self):
        add_snapshot(self.source, 1, b"def add(a,b): return a+b\n")
        with self.assertRaisesRegex(ValueError, "spec digest mismatch"):
            run_acceptance(self.source, self.suite, self.root / "output",
                           expected_spec_digest="0" * 64)
        self.assertFalse((self.root / "output").exists())

    def test_missing_isolation_blocks_execution(self):
        add_snapshot(self.source, 1, b"def add(a,b): return a+b\n")
        with patch("acceptance_runner.bubblewrap_available", return_value=False), \
             patch("acceptance_runner.run_process", side_effect=AssertionError("executed")):
            result = run_acceptance(self.source, self.suite, self.root / "output")
        self.assertEqual(result[0]["status"], "infrastructure_failure")
        self.assertEqual(grade(self.source, acceptance_dir=self.root / "output")["metrics"]["code_correctness"]["status"], "not_measured")

    def test_trusted_local_opt_in(self):
        result = self.run_one(trusted_local=True)
        self.assertEqual(result[0]["status"], "completed")
        self.assertEqual(result[0]["isolation"], "trusted_local")

    def test_timeout_and_test_execution_error(self):
        result = self.run_one(b"import time\ntime.sleep(30)\ndef add(a,b): return a+b\n")
        self.assertEqual(result[0]["status"], "timeout")
        self.assertEqual(grade(self.source, acceptance_dir=self.root / "output")["metrics"]["code_correctness"]["status"], "not_measured")
        second = self.root / "source2"
        second.mkdir()
        add_snapshot(second, 0, b"def add(a,b): return 0\n")
        add_snapshot(second, 1, b"raise RuntimeError('boom')\n")
        other = run_acceptance(second, self.suite, self.root / "output2")
        self.assertGreater(other[0]["counts"]["errors"], 0)
        self.assertEqual(grade(second, acceptance_dir=self.root / "output2")["metrics"]["code_correctness"]["status"], "measurement_error")

    def test_r1_r2_same_suite_improvement(self):
        add_snapshot(self.source, 1, b"def add(a,b): return 0\n")
        add_snapshot(self.source, 2, b"def add(a,b): return a+b\n")
        results = run_acceptance(self.source, self.suite, self.root / "output")
        self.assertEqual([r["round"] for r in results], [1, 2])
        graded = grade(self.source, acceptance_dir=self.root / "output")
        self.assertEqual(graded["metrics"]["revision_improvement"]["value"], 2)
        self.assertTrue(graded["metrics"]["code_correctness"]["value"])

    def test_partial_r2_evidence_does_not_score_r1_as_final(self):
        add_snapshot(self.source, 1, b"def add(a,b): return a+b\n")
        add_snapshot(self.source, 2, b"def add(a,b): return a+b\n")
        run_acceptance(self.source, self.suite, self.root / "output")
        (self.root / "output/r2.json").unlink()
        graded = grade(self.source, acceptance_dir=self.root / "output")
        self.assertEqual(graded["metrics"]["code_correctness"]["status"], "not_measured")

    def test_tampered_record_log_is_measurement_error(self):
        self.run_one()
        (self.root / "output/r1.log").write_text("tampered")
        result = grade(self.source, acceptance_dir=self.root / "output")
        self.assertEqual(result["metrics"]["code_correctness"]["status"], "measurement_error")


if __name__ == "__main__":
    unittest.main()
