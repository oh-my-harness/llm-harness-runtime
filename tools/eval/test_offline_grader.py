"""P1 离线评分器的临时产物测试。"""

import hashlib
import json
from pathlib import Path
import tempfile
import unittest

from offline_grader import grade, write_report


def snapshot(root: Path, round_no: int = 0, content: bytes = b"hello") -> tuple[Path, dict]:
    base = root / "workspace/.pipeline/snapshots/run-1" / f"r{round_no}"
    (base / "files").mkdir(parents=True)
    (base / "files" / "main.txt").write_bytes(content)
    aggregate = hashlib.sha256(f"8\x1fmain.txt\x1f{len(content)}\x1e".encode() + content).hexdigest()
    manifest = {
        "issue_id": "chat-1", "run_id": "run-1", "round": round_no,
        "changed_files": ["main.txt"],
        "file_hashes": {"main.txt": hashlib.sha256(content).hexdigest()},
        "content_hash": aggregate, "snapshot_id": f"chat-1#r{round_no}#{aggregate}",
    }
    if round_no:
        manifest.update(task_start_baseline=str(root / "workspace/.pipeline/snapshots/run-1/r0/snapshot.json"),
                        task_relative_changed_paths=["main.txt"], task_relative_diff="task-relative.patch")
        (base / "task-relative.patch").write_bytes(b"patch placeholder")
    path = base / "snapshot.json"
    path.write_text(json.dumps(manifest))
    return path, manifest


def events(root: Path, *items: dict) -> None:
    evidence = root / "evidence"
    evidence.mkdir(exist_ok=True)
    (evidence / "events.jsonl").write_text("".join(json.dumps(item) + "\n" for item in items))


class OfflineGraderTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "source"
        self.root.mkdir()

    def test_valid_and_corrupted_hashes(self) -> None:
        path, _ = snapshot(self.root)
        result = grade(self.root)
        self.assertEqual(result["metrics"]["snapshot_integrity"]["value"], True)
        (path.parent / "files/main.txt").write_bytes(b"damaged")
        result = grade(self.root)
        self.assertEqual(result["metrics"]["snapshot_integrity"]["status"], "measurement_error")
        self.assertIn("file hash mismatch", str(result["artifacts"]["snapshots"][0]["issues"]))

    def test_missing_snapshots_and_events(self) -> None:
        result = grade(self.root)
        self.assertEqual(result["metrics"]["snapshot_integrity"]["status"], "not_measured")
        self.assertEqual(result["artifacts"]["events"]["status"], "missing")
        self.assertEqual(result["outcome"]["category"], "unknown")

    def test_truncated_and_missing_terminal_events(self) -> None:
        events(self.root, {"type": "pipeline_started", "issue_id": "chat-1"})
        with (self.root / "evidence/events.jsonl").open("a") as stream:
            stream.write('{"type":')
        result = grade(self.root)
        self.assertEqual(result["artifacts"]["events"]["status"], "malformed")
        self.assertEqual(result["metrics"]["event_jsonl_parse"]["status"], "measurement_error")
        self.assertEqual(result["outcome"]["category"], "incomplete")

    def test_provider_blocked_and_unknown_usage(self) -> None:
        snapshot(self.root)
        events(self.root, {"type": "pipeline_started", "issue_id": "chat-1"},
               {"type": "agent_error", "error": "llm provider error: server error"},
               {"type": "pipeline_escalated", "reason": "timed out", "issue_id": "chat-1"})
        result = grade(self.root)
        self.assertEqual(result["outcome"]["category"], "provider_blocked")
        self.assertEqual(result["metrics"]["code_correctness"]["status"], "blocked")
        for name in ("tokens", "api_cost", "human_attention"):
            self.assertEqual(result["metrics"][name]["status"], "not_measured")
            self.assertNotIn("value", result["metrics"][name])

    def test_missing_r2_and_relationship(self) -> None:
        snapshot(self.root)
        snapshot(self.root, 1, b"new")
        result = grade(self.root)
        self.assertEqual(result["metrics"]["snapshot_relationship"]["value"], True)
        self.assertEqual(result["metrics"]["revision_improvement"]["status"], "not_measured")

    def test_r2_gap_and_aggregate_corruption(self) -> None:
        path, manifest = snapshot(self.root)
        snapshot(self.root, 2, b"new")
        self.assertIn("preceding round unavailable", grade(self.root)["metrics"]["snapshot_relationship"]["reason"])
        manifest["content_hash"] = "0" * 64
        manifest["snapshot_id"] = "chat-1#r0#" + manifest["content_hash"]
        path.write_text(json.dumps(manifest))
        result = grade(self.root)
        self.assertEqual(result["metrics"]["snapshot_integrity"]["status"], "measurement_error")
        self.assertIn("aggregate content_hash mismatch", str(result["artifacts"]["snapshots"][0]["issues"]))

    def test_unattributable_tests_and_valid_test_record(self) -> None:
        snapshot(self.root)
        _, revision = snapshot(self.root, 1, b"new")
        evidence = self.root / "evidence"
        evidence.mkdir()
        (evidence / "independent-test.txt").write_text("PASS\n")
        self.assertEqual(grade(self.root)["metrics"]["code_correctness"]["status"], "not_measured")
        (evidence / "independent-test.json").write_text('{"exit_code":0}')
        self.assertEqual(grade(self.root)["metrics"]["code_correctness"]["status"], "measurement_error")
        suite = evidence / "suite.py"
        suite.write_text("assert True\n")
        (evidence / "independent-test.json").write_text(json.dumps({
            "snapshot_id": revision["snapshot_id"], "suite_path": "evidence/suite.py",
            "suite_digest": hashlib.sha256(suite.read_bytes()).hexdigest(),
            "command": "python3 suite.py", "exit_code": 0,
        }))
        self.assertTrue(grade(self.root)["metrics"]["code_correctness"]["value"])

    def test_determinism_and_source_protection(self) -> None:
        snapshot(self.root)
        first = grade(self.root)
        self.assertEqual(first, grade(self.root))
        out = Path(self.temp.name) / "output"
        self.assertEqual(first, write_report(self.root, out))
        before = (out / "result.json").read_bytes()
        write_report(self.root, out)
        self.assertEqual(before, (out / "result.json").read_bytes())
        with self.assertRaises(ValueError):
            write_report(self.root, self.root / "report")

    def test_legacy_empty_manifest_is_unavailable(self) -> None:
        path, manifest = snapshot(self.root)
        manifest.pop("file_hashes")
        manifest["changed_files"] = []
        path.write_text(json.dumps(manifest))
        result = grade(self.root)
        self.assertEqual(result["artifacts"]["snapshots"][0]["status"], "unavailable")
        self.assertEqual(result["metrics"]["snapshot_integrity"]["status"], "not_measured")


if __name__ == "__main__":
    unittest.main()
