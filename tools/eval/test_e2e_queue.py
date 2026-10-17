"""Non-billable queue tests with synthetic Studio API behavior."""

import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
from pathlib import Path
import shutil
import sqlite3
import stat
import subprocess
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

from e2e_queue import QueueStore, StudioSession, baseline_digest, prepare, run_queue, tree_digest, journal_workflow
from studio_collector import RuntimeApi
from test_acceptance_runner import add_snapshot, make_spec


class FakeApi:
    def __init__(self, session):
        self.session = session
        self.repo = None
        self.team = None
        self.terminal = None

    def post(self, path, payload, **kwargs):
        self.session.calls.append(path)
        if path == "/api/team/fs/grants":
            self.repo = Path(payload["path"])
            return {"id": "grant"}
        if path == "/api/team/projects":
            self.team = payload["id"]
            assert payload["template_id"] == "coding-team"
            assert payload["repo_path"] == str(self.repo)
            target = self.session.data_root / "teams" / self.team
            target.mkdir(parents=True)
            (target / "team.json").write_text(json.dumps({"id": self.team,
                "sandbox_mode": self.session.sandbox_mode, "members": [
                {"id": "orchestrator", "persona": "effective prompt", "model": "strong"}]}))
            return {"ok": True}
        if path == "/api/team/chat":
            assert self.session.ready_before_chat
            assert payload["target"] == "team" and payload["project"] == self.team
            self.session.starting_workspace_bytes = (self.repo / "calc.py").read_bytes()
            issue = "chat-" + self.team
            if "silent" in payload["text"]:
                self.session.collector.process_event(json.dumps({"type": "pipeline_started", "project": self.team,
                                                                  "issue_id": issue}))
                return {"ok": True, "workflow_id": issue}
            add_snapshot(self.repo.parent, 0, b"def add(a,b): return 0\n", issue)
            kind = "pipeline_escalated" if "provider-fail" in payload["text"] else "pipeline_completed"
            if kind == "pipeline_completed":
                code = b"def add(a,b): return 0\n" if "wrong-code" in payload["text"] else b"def add(a,b): return a+b\n"
                add_snapshot(self.repo.parent, 1, code, issue)
                if "corrupt" in payload["text"]:
                    (self.repo / ".pipeline/snapshots/run-1/r1/files/calc.py").write_text("bad")
            self.terminal = {"message_id": f"{issue}:terminal", "broadcast_id": issue,
                             "text": "provider error" if kind == "pipeline_escalated" else "FINISH"}
            self.session.collector.process_event(json.dumps({"type": "pipeline_started", "project": self.team,
                                                              "issue_id": issue}))
            if kind == "pipeline_escalated":
                self.session.collector.process_event(json.dumps({"type": "agent_error", "project": self.team,
                                                                  "issue_id": issue, "error": "provider error"}))
            self.session.collector.process_event(json.dumps({"type": kind, "project": self.team,
                                                              "issue_id": issue}))
            self.session.collector.process_event(json.dumps({"type": "team_message", "project": self.team,
                                                              "broadcast_id": issue,
                                                              "message_id": f"{issue}:terminal", "text": self.terminal["text"]}))
            if self.session.ambiguous:
                raise OSError("reply lost after admission")
            return {"ok": True, "workflow_id": issue}
        raise AssertionError(path)

    def get(self, path, **kwargs):
        assert path == "/api/team/startup"
        return {"execution_mode": self.session.execution_mode}

    def project_repo(self, project):
        return str(self.repo)

    def terminal_message(self, project, issue):
        return self.terminal if self.terminal and self.terminal["broadcast_id"] == issue else None


class FakeSession:
    instances = []
    ambiguous = False
    sandbox_mode = "bwrap"
    execution_mode = "evaluation"

    def __init__(self, binary, data_root, variant, key_file):
        self.data_root = data_root
        self.calls = []
        self.ready_before_chat = False
        self.starting_workspace_bytes = None
        self.api = FakeApi(self)
        self.collector = None
        self.sandbox_mode = type(self).sandbox_mode
        self.execution_mode = type(self).execution_mode
        self.ambiguous = type(self).ambiguous
        type(self).instances.append(self)

    def start(self, deadline):
        self.data_root.mkdir(parents=True, exist_ok=True)
        return self.api

    def ready(self, deadline):
        self.calls.append("ready")
        self.ready_before_chat = True

    def collect(self, collector):
        self.collector = collector

    def drain(self, collector):
        pass

    def close(self):
        self.calls.append("closed")


class QueueTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.fixture = self.root / "fixture-repo"
        self.fixture.mkdir()
        (self.fixture / "calc.py").write_bytes(b"def add(a,b): return 0\n")
        subprocess.run(["git", "init", "-q"], cwd=self.fixture, check=True)
        self.spec = make_spec(self.root, "import unittest\nfrom calc import add\n"
                              "class TestAdd(unittest.TestCase):\n"
                              " def test_add(self): self.assertEqual(add(1,2),3)\n")
        self.prompt = self.spec.parent / "fixture.txt"
        self.variant = self.root / "variant.json"
        self.variant.write_text(json.dumps({"id": "mock-v1", "models": {"strong": "mock-a",
                                 "main": "mock-b", "cheap": "mock-c"}, "base_url": "http://localhost:1"}))
        self.key = self.root / "key"
        self.key.write_text("test-only-key")
        self.store = QueueStore(self.root / "queue")
        FakeSession.instances = []
        FakeSession.ambiguous = False
        FakeSession.sandbox_mode = "bwrap"
        FakeSession.execution_mode = "evaluation"

    def add_task(self, task_id, prompt=None):
        if prompt is not None:
            folder = self.root / f"acceptance-{task_id}"
            folder.mkdir()
            (folder / "fixture.txt").write_text(prompt)
            suite = self.spec.parent / "test_acceptance.py"
            (folder / "test_acceptance.py").write_bytes(suite.read_bytes())
            (folder / "spec.json").write_text(json.dumps({"fixture_id": "arithmetic",
                "fixture_version": "1", "fixture_path": "fixture.txt",
                "fixture_digest": hashlib.sha256(prompt.encode()).hexdigest(),
                "suite_version": "1", "suite_path": "test_acceptance.py",
                "suite_digest": hashlib.sha256(suite.read_bytes()).hexdigest(), "timeout_seconds": 2}))
            spec = folder / "spec.json"
            prompt_path = folder / "fixture.txt"
        else:
            spec, prompt_path = self.spec, self.prompt
        task_file = self.root / f"{task_id}.json"
        prepare(task_id, "1", self.fixture, prompt_path, spec, self.variant, task_file)
        return self.store.enqueue(task_file)

    def run_tasks(self, **kwargs) -> int:
        return run_queue(self.store, binary=Path("/does/not/run"), key_file=self.key,
                         max_attempts=kwargs.pop("max_attempts", 10),
                         max_duration=kwargs.pop("max_duration", 30),
                         attempt_timeout=kwargs.pop("attempt_timeout", 10),
                         session_factory=FakeSession, **kwargs)

    def test_confinement_unverified_blocks_before_chat(self):
        """Fail-closed admission: os-fallback / missing / unknown sandbox_mode
        must be rejected before any chat submission."""
        for mode in ("os-fallback", "trusted-host", None, "quantum"):
            with self.subTest(mode=mode):
                FakeSession.instances = []
                FakeSession.sandbox_mode = mode
                task_id = f"conf-{mode or 'missing'}"
                self.add_task(task_id)
                self.run_tasks()
                row = self.store.one(task_id)
                self.assertEqual(row["state"], "blocked", f"mode={mode!r}")
                self.assertEqual(row["classification"], "confinement_unverified",
                                 f"mode={mode!r}")
                self.assertIn("sandbox_mode", row["error"] or "")
                self.assertFalse(list((self.store.root / "attempts").glob(
                    "*/evaluation/runs/*/result.json")),
                    "no attempt may be graded after a confinement-admission failure")
        # No chat submission across all rejection cases.
        self.assertEqual(sum(s.calls.count("/api/team/chat") for s in FakeSession.instances
                             if s.sandbox_mode != "bwrap"), 0,
                         "chat must never be submitted for unverified confinement")
        FakeSession.sandbox_mode = "bwrap"

    def test_development_runtime_blocks_before_registration(self):
        for mode in ("development", None):
            FakeSession.execution_mode = mode
            FakeSession.instances = []
            task_id = f"runtime-{mode or 'missing'}"
            self.add_task(task_id)
            self.run_tasks()
            self.assertEqual(self.store.one(task_id)["classification"], "confinement_unverified")
            self.assertTrue(all("/api/team/projects" not in s.calls for s in FakeSession.instances))
            self.assertTrue(all("/api/team/chat" not in s.calls for s in FakeSession.instances))
        FakeSession.execution_mode = "evaluation"

    def test_confinement_verified_allows_submission(self):
        self.add_task("ok-bwrap")
        self.run_tasks()
        self.assertEqual(self.store.one("ok-bwrap")["state"], "completed")

    def test_preparation_persistence_and_sequential_isolation(self):
        self.add_task("one")
        self.add_task("two")
        self.assertEqual([r["state"] for r in QueueStore(self.store.root).rows()], ["queued", "queued"])
        self.run_tasks()
        rows = self.store.rows()
        self.assertEqual([r["state"] for r in rows], ["completed", "completed"])
        self.assertNotEqual(rows[0]["attempt_id"], rows[1]["attempt_id"])
        self.assertEqual(len({str(s.data_root) for s in FakeSession.instances}), 2)
        for session, row in zip(FakeSession.instances, rows):
            self.assertEqual(session.starting_workspace_bytes, b"def add(a,b): return 0\n")
            self.assertLess(session.calls.index("ready"), session.calls.index("/api/team/chat"))
            self.assertEqual(session.calls[:2], ["/api/team/fs/grants", "/api/team/projects"])
            result = json.loads((Path(row["artifact_dir"]) / "result.json").read_text())
            self.assertTrue(result["metrics"]["code_correctness"]["value"])
            attempt = self.store.root / "attempts" / row["attempt_id"]
            self.assertTrue((attempt / "effective-team.json").is_file())
            self.assertIn("configuration_fingerprint", json.loads((attempt / "metadata.json").read_text()))
        fingerprints = [json.loads((self.store.root / "attempts" / row["attempt_id"] /
                                     "metadata.json").read_text())["configuration_fingerprint"] for row in rows]
        self.assertEqual(fingerprints[0], fingerprints[1])
        self.assertEqual(tree_digest(Path(json.loads(Path(rows[0]["definition"]).read_text())["fixture_dir"])),
                         json.loads(Path(rows[0]["definition"]).read_text())["fixture_tree_digest"])

    def test_real_session_uses_prepared_private_directory_without_retry(self):
        self.add_task("local-setup")
        stub = self.root / "studio-stub"
        stub.write_text("#!/bin/sh\nexit 0\n")
        stub.chmod(0o700)
        def run_local():
            run_queue(self.store, binary=stub, key_file=self.key, max_attempts=1,
                      max_duration=5, attempt_timeout=3)
        run_local()
        row = self.store.one("local-setup")
        self.assertEqual(row["state"], "failed")
        self.assertIn("Studio process exited during startup", row["error"])
        self.assertNotIn("FileExistsError", row["error"])
        runtime = self.store.root / "runtime-state" / row["attempt_id"]
        attempt = Path(row["artifact_dir"])
        self.assertEqual(stat.S_IMODE(runtime.stat().st_mode), 0o700)
        self.assertTrue((runtime / "settings.json").is_file())
        self.assertTrue((runtime / "studio.log").is_file())
        self.assertFalse((runtime / "apikey").exists())
        metadata = (attempt / "metadata.json").read_bytes()
        self.assertTrue((attempt / "workspace/calc.py").is_file())
        run_local()
        self.assertEqual(self.store.one("local-setup")["attempt_id"], row["attempt_id"])
        self.assertEqual(len(list((self.store.root / "attempts").iterdir())), 1)
        self.assertEqual((attempt / "metadata.json").read_bytes(), metadata)

    def test_real_session_rejects_unsafe_existing_runtime_directory(self):
        target = self.root / "target"
        target.mkdir(mode=0o700)
        candidates = [self.root / "file", self.root / "link", self.root / "public"]
        candidates[0].write_text("not a directory")
        candidates[1].symlink_to(target, target_is_directory=True)
        candidates[2].mkdir(mode=0o755)
        candidates[2].chmod(0o755)
        for path in candidates:
            with self.subTest(path=path.name):
                session = StudioSession(self.root / "missing-binary", path,
                                        {"models": {"strong": "mock"}}, self.key)
                with self.assertRaises(ValueError):
                    session.start(time.monotonic() + 1)
                self.assertFalse((target / "settings.json").exists())
                self.assertFalse((path / "settings.json").exists())

    def test_max_attempts_leaves_next_task_queued_for_restart(self):
        self.add_task("first")
        self.add_task("second")
        self.run_tasks(max_attempts=1)
        self.assertEqual([row["state"] for row in self.store.rows()], ["completed", "queued"])
        self.run_tasks(max_attempts=1)
        self.assertEqual([row["state"] for row in self.store.rows()], ["completed", "completed"])

    def test_enqueue_recovers_complete_package_after_database_interruption(self):
        self.add_task("recover-enqueue")
        with sqlite3.connect(self.store.db) as conn:
            conn.execute("DELETE FROM tasks WHERE task_id='recover-enqueue'")
        self.assertEqual(self.store.enqueue(self.root / "recover-enqueue.json"), "recover-enqueue")
        self.assertEqual(self.store.one("recover-enqueue")["state"], "queued")

    def test_same_variant_name_with_changed_models_has_different_fingerprint(self):
        self.add_task("variant-a")
        changed = json.loads(self.variant.read_text())
        changed["models"]["main"] = "mock-other"
        self.variant.write_text(json.dumps(changed))
        self.add_task("variant-b")
        self.run_tasks()
        rows = self.store.rows()
        fingerprints = [json.loads((self.store.root / "attempts" / row["attempt_id"] /
                                     "metadata.json").read_text())["configuration_fingerprint"] for row in rows]
        self.assertNotEqual(*fingerprints)

    def test_provider_failure_is_blocked_not_code_failure(self):
        self.add_task("provider", "provider-fail")
        self.run_tasks()
        row = self.store.one("provider")
        self.assertEqual(row["state"], "blocked")
        self.assertEqual(row["classification"], "provider_blocked")
        result = json.loads((Path(row["artifact_dir"]) / "result.json").read_text())
        self.assertEqual(result["metrics"]["code_correctness"]["status"], "blocked")

    def test_ambiguous_reply_never_resubmits(self):
        self.add_task("ambiguous")
        FakeSession.ambiguous = True
        self.run_tasks()
        self.assertEqual(sum(s.calls.count("/api/team/chat") for s in FakeSession.instances), 1)
        self.assertEqual(self.store.one("ambiguous")["state"], "completed")
        self.run_tasks()
        self.assertEqual(sum(s.calls.count("/api/team/chat") for s in FakeSession.instances), 1)

    def test_sandbox_unavailable_and_corrupted_evidence(self):
        self.add_task("sandbox")
        with patch("acceptance_runner.bubblewrap_available", return_value=False):
            self.run_tasks()
        row = self.store.one("sandbox")
        self.assertEqual(row["state"], "blocked")
        self.assertEqual(row["classification"], "acceptance_unavailable")
        result = json.loads((Path(row["artifact_dir"]) / "result.json").read_text())
        self.assertEqual(result["metrics"]["code_correctness"]["status"], "not_measured")
        self.add_task("corrupt", "corrupt")
        self.run_tasks()
        row = self.store.one("corrupt")
        self.assertEqual(row["state"], "blocked")
        self.assertEqual(row["classification"], "evidence_corrupt")
        result = json.loads((Path(row["artifact_dir"]) / "result.json").read_text())
        self.assertEqual(result["metrics"]["snapshot_integrity"]["status"], "measurement_error")

    def test_interrupted_attempt_is_not_replayed(self):
        self.add_task("interrupted")
        self.store.transition("interrupted", "running", attempt_id="00000000-0000-0000-0000-000000000000",
                              classification="submission_uncertain")
        self.run_tasks()
        self.assertEqual(self.store.one("interrupted")["state"], "interrupted")
        self.assertEqual(FakeSession.instances, [])

    def test_restart_recovers_persisted_terminal_without_replay(self):
        self.add_task("recover")
        attempt_id = "00000000-0000-0000-0000-000000000001"
        team = "eval-" + attempt_id.replace("-", "")[:24]
        issue = "chat-recovered"
        attempt = self.store.root / "attempts" / attempt_id
        workspace = attempt / "workspace"
        shutil.copytree(self.fixture, workspace)
        add_snapshot(attempt, 0, b"def add(a,b): return 0\n", issue)
        add_snapshot(attempt, 1, b"def add(a,b): return a+b\n", issue)
        journal = self.store.root / "runtime-state" / attempt_id / "teams" / team / "chat.sqlite3"
        journal.parent.mkdir(parents=True)
        with sqlite3.connect(journal) as conn:
            conn.execute("CREATE TABLE chat_messages (id INTEGER PRIMARY KEY, payload TEXT)")
            for message in ({"from": "operator", "to": "team", "body": self.prompt.read_text(),
                             "broadcast_id": issue},
                            {"message_id": f"{issue}:terminal", "broadcast_id": issue,
                             "body": "FINISH"}):
                conn.execute("INSERT INTO chat_messages(payload) VALUES(?)", (json.dumps(message),))
        self.store.transition("recover", "running", attempt_id=attempt_id,
                              classification="submission_uncertain")
        self.run_tasks()
        row = self.store.one("recover")
        self.assertEqual(row["state"], "blocked")
        self.assertEqual(row["workflow_id"], issue)
        self.assertTrue((Path(row["artifact_dir"]) / "result.json").is_file())
        self.assertEqual(FakeSession.instances, [])

    def test_attempt_timeout_is_not_code_failure(self):
        self.add_task("timeout", "silent")
        self.run_tasks(attempt_timeout=1, max_duration=2)
        row = self.store.one("timeout")
        self.assertEqual(row["state"], "blocked")
        self.assertEqual(row["classification"], "timed_out")
        self.assertEqual(FakeSession.instances[0].calls.count("/api/team/chat"), 1)

    def test_allowlist_blocks_before_provider_execution(self):
        self.add_task("disallowed")
        self.run_tasks(allow_models={"other"})
        self.assertEqual(self.store.one("disallowed")["state"], "blocked")
        self.assertEqual(FakeSession.instances, [])

    def test_exit_code_success_for_completed_evaluation(self):
        self.add_task("ok")
        self.assertEqual(self.run_tasks(), 0)
        row = self.store.one("ok")
        self.assertEqual(row["state"], "completed")
        self.assertEqual(row["classification"], "completed")

    def test_exit_code_zero_for_code_quality_failure(self):
        # Generated code that fails acceptance tests is a completed evaluation
        # with measured failing correctness, not an infrastructure failure.
        self.add_task("quality", "wrong-code")
        self.assertEqual(self.run_tasks(), 0)
        row = self.store.one("quality")
        self.assertEqual(row["state"], "completed")
        self.assertEqual(row["classification"], "completed")
        result = json.loads((Path(row["artifact_dir"]) / "result.json").read_text())
        self.assertEqual(result["outcome"]["category"], "completed")
        self.assertEqual(result["metrics"]["code_correctness"]["status"], "measured")
        self.assertIs(result["metrics"]["code_correctness"]["value"], False)

    def test_exit_code_nonzero_for_infrastructure_failure(self):
        self.add_task("broken")
        stub = self.root / "studio-stub"
        stub.write_text("#!/bin/sh\nexit 0\n")
        stub.chmod(0o700)

        def run_local():
            return run_queue(self.store, binary=stub, key_file=self.key, max_attempts=1,
                             max_duration=5, attempt_timeout=3)

        self.assertEqual(run_local(), 1)
        row = self.store.one("broken")
        self.assertEqual(row["state"], "failed")
        self.assertEqual(row["classification"], "infrastructure_failed")

    def test_exit_code_zero_for_provider_blocked_outcome(self):
        self.add_task("provider", "provider-fail")
        self.assertEqual(self.run_tasks(), 0)
        row = self.store.one("provider")
        self.assertEqual(row["state"], "blocked")
        self.assertEqual(row["classification"], "provider_blocked")

    def test_real_http_request_uses_production_chat_route_and_auth(self):
        seen = []
        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                seen.append((self.path, self.headers.get("Authorization"), json.loads(body)))
                response = b'{"ok":true,"workflow_id":"chat-1"}'
                self.send_response(202)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(response)))
                self.end_headers()
                self.wfile.write(response)
            def log_message(self, *_):
                pass
        server = HTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        try:
            panel = self.root / "panel.json"
            panel.write_text(json.dumps({"url": f"http://127.0.0.1:{server.server_port}/#token=test-token"}))
            response = RuntimeApi(panel).post("/api/team/chat", {"project": "p", "target": "team",
                                                              "text": "objective"})
            self.assertEqual(response["workflow_id"], "chat-1")
            self.assertEqual(seen, [("/api/team/chat", "Bearer test-token",
                                     {"project": "p", "target": "team", "text": "objective"})])
        finally:
            server.shutdown()
            thread.join()
            server.server_close()


if __name__ == "__main__":
    unittest.main()
