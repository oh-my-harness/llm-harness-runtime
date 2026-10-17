"""Studio collector 的合成事件与故障恢复测试。"""

import hashlib
import json
from pathlib import Path
import socket
import tempfile
import threading
import unittest
import shutil
from unittest.mock import patch
import base64

from studio_collector import LiveSocket, RunCollector, RuntimeApi
from test_acceptance_runner import add_snapshot as add_code_snapshot, make_spec


class FakeApi:
    def __init__(self):
        self.repos = {}
        self.terminals = {}

    def project_repo(self, project):
        return self.repos.get(project)

    def terminal_message(self, project, issue):
        return self.terminals.get((project, issue))


def emit(collector, project, issue, kind, **extra):
    event = {"project": project, "issue_id": issue, "type": kind, **extra}
    collector.process_event(json.dumps(event, separators=(",", ":")))


def terminal_chat(collector, project, issue):
    collector.process_event(json.dumps({"type": "team_message", "project": project,
                                        "broadcast_id": issue, "message_id": f"{issue}:terminal",
                                        "text": "## Human review package\n"}))


def add_snapshot(repo: Path, issue: str):
    folder = repo / ".pipeline/snapshots/run-1/r0"
    (folder / "files").mkdir(parents=True)
    content = b"hello"
    (folder / "files/main.txt").write_bytes(content)
    aggregate = hashlib.sha256(b"8\x1fmain.txt\x1f5\x1ehello").hexdigest()
    (folder / "snapshot.json").write_text(json.dumps({
        "issue_id": issue, "run_id": "run-1", "round": 0,
        "changed_files": ["main.txt"],
        "file_hashes": {"main.txt": hashlib.sha256(content).hexdigest()},
        "content_hash": aggregate, "snapshot_id": f"{issue}#r0#{aggregate}",
    }))
    return aggregate


class CollectorTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.api = FakeApi()
        self.output = self.root / "evaluations"
        self.collector = RunCollector(self.output, self.api)

    def record(self, project="alpha", issue="chat-1"):
        return self.collector.runs[(project, issue)]

    def finish(self, project="alpha", issue="chat-1", kind="pipeline_completed"):
        self.api.terminals[(project, issue)] = {"message_id": f"{issue}:terminal",
                                                "broadcast_id": issue, "text": "## Human review package\n"}
        emit(self.collector, project, issue, kind, reason="done")
        terminal_chat(self.collector, project, issue)

    def test_successful_workflow_and_snapshot_grading(self):
        repo = self.root / "repo"
        digest = add_snapshot(repo, "chat-1")
        self.api.repos["alpha"] = str(repo)
        self.collector = RunCollector(self.output, self.api, {"projects": {"alpha": {
            "fixture_id": "f", "fixture_version": "1", "fixture_digest": "fixture-digest",
            "baseline_digest": digest, "suite_digest": "suite-digest", "variant_id": "v",
        }}})
        emit(self.collector, "alpha", "chat-1", "pipeline_started")
        self.finish()
        record = self.record()
        self.assertEqual(record["state"], "graded")
        self.assertEqual(record["mode"], "controlled_metadata_only")
        result = json.loads((self.collector._directory(record) / "result.json").read_text())
        self.assertEqual(result["outcome"]["category"], "completed")
        self.assertTrue(result["metrics"]["snapshot_integrity"]["value"])
        self.assertEqual(result["metrics"]["code_correctness"]["status"], "not_measured")

    def test_verified_suite_triggers_acceptance_after_terminal(self):
        staging = self.root / "staging"
        staging.mkdir()
        add_code_snapshot(staging, 0, b"def add(a,b): return 0\n")
        add_code_snapshot(staging, 1, b"def add(a,b): return a+b\n")
        repo = self.root / "repo"
        shutil.copytree(staging / "workspace/.pipeline", repo / ".pipeline")
        baseline = json.loads((repo / ".pipeline/snapshots/run-1/r0/snapshot.json").read_text())["content_hash"]
        spec_path = make_spec(self.root, "import unittest\nfrom calc import add\n"
                              "class TestAdd(unittest.TestCase):\n"
                              " def test_add(self): self.assertEqual(add(1,2),3)\n")
        spec = json.loads(spec_path.read_text())
        self.api.repos["alpha"] = str(repo)
        self.collector = RunCollector(self.output, self.api, {"projects": {"alpha": {
            "fixture_id": "arithmetic", "fixture_version": "1", "fixture_digest": spec["fixture_digest"],
            "baseline_digest": baseline, "suite_digest": spec["suite_digest"], "variant_id": "v1",
            "acceptance_spec": str(spec_path),
            "acceptance_spec_digest": hashlib.sha256(spec_path.read_bytes()).hexdigest(),
        }}})
        emit(self.collector, "alpha", "chat-1", "pipeline_started")
        self.finish()
        record = self.record()
        self.assertEqual(record["state"], "graded")
        result = json.loads((self.collector._directory(record) / "result.json").read_text())
        self.assertTrue(result["metrics"]["code_correctness"]["value"])
        self.assertTrue((self.collector._directory(record) / "acceptance/r1.json").is_file())

    def test_declared_baseline_mismatch_stays_ordinary(self):
        repo = self.root / "repo"
        add_snapshot(repo, "chat-1")
        self.api.repos["alpha"] = str(repo)
        self.collector = RunCollector(self.output, self.api, {"projects": {"alpha": {
            "fixture_id": "f", "fixture_version": "1", "fixture_digest": "fixture-digest",
            "baseline_digest": "wrong", "suite_digest": "suite-digest", "variant_id": "v",
        }}})
        emit(self.collector, "alpha", "chat-1", "pipeline_started")
        self.finish()
        self.assertEqual(self.record()["mode"], "ordinary")
        self.assertIn("declared baseline digest does not match a verified captured r0",
                      self.record()["comparability_issues"])

    def test_ingress_before_start_keeps_arrival_order(self):
        self.collector.process_event(json.dumps({"type": "team_message", "project": "alpha",
                                                 "broadcast_id": "chat-1", "message_id": "m1"}))
        emit(self.collector, "alpha", "chat-1", "pipeline_started")
        record = self.record()
        events = (self.collector._directory(record) / "source/evidence/events.jsonl").read_text().splitlines()
        self.assertEqual([json.loads(line)["type"] for line in events],
                         ["team_message", "pipeline_started"])

    def test_provider_blocked(self):
        emit(self.collector, "alpha", "chat-1", "pipeline_started")
        self.collector.process_event(json.dumps({"project": "alpha", "type": "agent_error",
                                                  "error": "llm provider error"}))
        self.finish(kind="pipeline_escalated")
        result = json.loads((self.collector._directory(self.record()) / "result.json").read_text())
        self.assertEqual(result["outcome"]["category"], "provider_blocked")
        self.assertEqual(result["metrics"]["code_correctness"]["status"], "blocked")

    def test_sequential_and_overlapping_runs_do_not_mix(self):
        for project, issue in (("alpha", "chat-1"), ("beta", "chat-2")):
            emit(self.collector, project, issue, "pipeline_started")
        self.finish("beta", "chat-2")
        self.finish("alpha", "chat-1")
        for project, issue in (("alpha", "chat-1"), ("beta", "chat-2")):
            record = self.record(project, issue)
            raw = (self.collector._directory(record) / "source/evidence/events.jsonl").read_text()
            self.assertIn(issue, raw)
            self.assertNotIn("chat-2" if issue == "chat-1" else "chat-1", raw)
        emit(self.collector, "alpha", "chat-3", "pipeline_started")
        self.finish("alpha", "chat-3")
        self.assertEqual(len({str(self.collector._directory(r)) for r in self.collector.runs.values()}), 3)

    def test_duplicate_terminal_does_not_regrade(self):
        emit(self.collector, "alpha", "chat-1", "pipeline_started")
        self.finish()
        record = self.record()
        result_path = self.collector._directory(record) / "result.json"
        before = result_path.read_bytes()
        event_path = self.collector._directory(record) / "source/evidence/events.jsonl"
        line_count = len(event_path.read_text().splitlines())
        emit(self.collector, "alpha", "chat-1", "pipeline_completed")
        terminal_chat(self.collector, "alpha", "chat-1")
        self.assertEqual(before, result_path.read_bytes())
        self.assertEqual(line_count, len(event_path.read_text().splitlines()))

    def test_restart_recovers_known_run_with_gap(self):
        emit(self.collector, "alpha", "chat-1", "pipeline_started")
        emit(self.collector, "alpha", "chat-1", "pipeline_completed")
        self.api.terminals[("alpha", "chat-1")] = {"text": "terminal"}
        self.collector = RunCollector(self.output, self.api)
        self.collector.reconcile()
        self.assertEqual(self.record()["state"], "graded")
        self.assertIn("collector restarted before grading", self.record()["collection_gaps"])

    def test_incomplete_history_is_marked(self):
        emit(self.collector, "alpha", "chat-1", "pipeline_started")
        self.collector.mark_disconnected()
        self.api.terminals[("alpha", "chat-1")] = {"text": "terminal"}
        self.collector.reconcile()
        record = self.record()
        result = json.loads((self.collector._directory(record) / "result.json").read_text())
        self.assertEqual(result["outcome"]["category"], "incomplete")
        self.assertTrue(record["collection_gaps"])

    def test_runtime_channel_lag_is_marked(self):
        emit(self.collector, "alpha", "chat-1", "pipeline_started")
        self.collector.mark_stream_gap(7)
        self.assertIn("Runtime event channel dropped 7 events", self.record()["collection_gaps"])

    def test_output_inside_workspace_is_rejected(self):
        self.api.repos["alpha"] = str(self.root)
        collector = RunCollector(self.root / "evaluations", self.api)
        emit(collector, "alpha", "chat-1", "pipeline_started")
        self.assertNotIn(("alpha", "chat-1"), collector.runs)

    def test_delayed_terminal_persistence(self):
        emit(self.collector, "alpha", "chat-1", "pipeline_started")
        emit(self.collector, "alpha", "chat-1", "pipeline_completed")
        terminal_chat(self.collector, "alpha", "chat-1")
        self.assertNotEqual(self.record()["state"], "graded")
        self.api.terminals[("alpha", "chat-1")] = {"text": "terminal"}
        self.collector.reconcile()
        self.assertEqual(self.record()["state"], "graded")

    def test_grader_failure_preserves_source_and_retries(self):
        emit(self.collector, "alpha", "chat-1", "pipeline_started")
        self.api.terminals[("alpha", "chat-1")] = {"text": "terminal"}
        emit(self.collector, "alpha", "chat-1", "pipeline_completed")
        with patch("studio_collector.write_report", side_effect=RuntimeError("grader failed")):
            terminal_chat(self.collector, "alpha", "chat-1")
        record = self.record()
        self.assertEqual(record["state"], "grade_failed")
        self.assertTrue((self.collector._directory(record) / "source/evidence/events.jsonl").is_file())
        self.collector.reconcile()
        self.assertEqual(record["state"], "graded")

    def test_late_discovery_marks_missing_start(self):
        self.api.terminals[("alpha", "chat-1")] = {"text": "terminal"}
        emit(self.collector, "alpha", "chat-1", "pipeline_escalated", reason="timed out")
        self.assertIn("pipeline_started was not observed", self.record()["collection_gaps"])
        self.assertEqual(self.record()["state"], "graded")

    def test_local_websocket_auth_and_ready_frame(self):
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        port = listener.getsockname()[1]
        descriptor = self.root / "panel.json"
        descriptor.write_text(json.dumps({"url": f"http://127.0.0.1:{port}/#token=test-token"}))
        seen = []

        def server():
            try:
                client, _ = listener.accept()
                with client:
                    request = bytearray()
                    while b"\r\n\r\n" not in request:
                        request.extend(client.recv(4096))
                    seen.append(bytes(request))
                    key = next(line.split(b":", 1)[1].strip() for line in request.split(b"\r\n")
                               if line.lower().startswith(b"sec-websocket-key:"))
                    accept = base64.b64encode(hashlib.sha1(key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest())
                    client.sendall(b"HTTP/1.1 101 Switching Protocols\r\n"
                                   b"sec-websocket-accept: " + accept + b"\r\n\r\n"
                                   b"\x81\x1d{\"type\":\"event_stream_ready\"}")
            finally:
                listener.close()

        thread = threading.Thread(target=server)
        thread.start()
        client = LiveSocket(RuntimeApi(descriptor))
        self.assertEqual(json.loads(client.receive())["type"], "event_stream_ready")
        client.close()
        thread.join(timeout=2)
        self.assertIn(b"GET /api/team/events?live_only=true", seen[0])
        self.assertIn(b"Authorization: Bearer test-token", seen[0])


if __name__ == "__main__":
    unittest.main()
