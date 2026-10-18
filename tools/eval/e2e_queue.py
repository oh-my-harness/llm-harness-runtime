"""Durable sequential queue for production Coding Team evaluations."""

from __future__ import annotations

import argparse
from contextlib import contextmanager
from datetime import datetime, timezone
import fcntl
import hashlib
import json
import os
from pathlib import Path
import queue
import shutil
import signal
import socket
import sqlite3
import stat
import subprocess
import sys
import threading
import time
import uuid
from typing import Any
from urllib.parse import urlsplit

from acceptance_runner import RUNNER_VERSION, load_spec
from offline_grader import GRADER_VERSION, grade, safe_rel
from studio_collector import LiveSocket, RunCollector, RuntimeApi
from variant import template_prompt_digests

REPO = Path(__file__).resolve().parents[2]
DEFAULT_TEMPLATE = REPO / "crates/llm-harness-agent-team-studio/templates/coding-team"
FINAL = {"completed", "blocked", "failed", "interrupted"}


class ConfinementNotVerified(Exception):
    """Effective execution environment is not verified as sandboxed."""


def stamp() -> str:
    return datetime.now(timezone.utc).isoformat()


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def check_task_id(task_id: str) -> None:
    if (not isinstance(task_id, str) or not task_id or len(task_id) > 64 or
            not task_id[0].isascii() or not task_id[0].isalnum() or
            not all(c.isascii() and (c.isalnum() or c in "_-") for c in task_id)):
        raise ValueError("task id must start with a letter/digit and contain only ASCII letters, digits, _ or -")


def canonical(value: Any) -> bytes:
    return json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode()


def save_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_bytes(json.dumps(value, indent=2, ensure_ascii=False, sort_keys=True).encode() + b"\n")
    os.replace(temporary, path)


def tree_digest(root: Path) -> str:
    if not root.is_dir() or root.is_symlink():
        raise ValueError(f"unsafe fixture/template directory: {root}")
    aggregate = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        if path.is_symlink() or not (path.is_dir() or path.is_file()):
            raise ValueError(f"unsupported fixture/template entry: {path}")
        if path.is_file():
            rel = path.relative_to(root).as_posix()
            if not safe_rel(rel):
                raise ValueError(f"unsafe fixture/template path: {rel}")
            data = path.read_bytes()
            aggregate.update(f"{len(rel.encode())}\x1f{rel}\x1f{len(data)}\x1e".encode())
            aggregate.update(data)
    return aggregate.hexdigest()


def validate_base_url(value: str) -> None:
    if not value:
        return
    parsed = urlsplit(value)
    if (parsed.scheme not in ("http", "https") or not parsed.hostname or parsed.username or
            parsed.password or parsed.query or parsed.fragment):
        raise ValueError("base_url must be a credential-free HTTP(S) origin")


def baseline_digest(fixture: Path) -> str:
    if not (fixture / ".git").is_dir() or (fixture / ".git").is_symlink():
        raise ValueError("fixture must contain a standalone .git directory")
    if (fixture / ".pipeline").exists():
        raise ValueError("fixture must not contain prior Runtime Snapshot state")
    check = subprocess.run(["git", "rev-parse", "--show-toplevel"], cwd=fixture,
                           capture_output=True, check=True, timeout=60)
    if Path(check.stdout.decode().strip()).resolve() != fixture.resolve():
        raise ValueError("fixture must be the Git repository root")
    ignored = subprocess.run(["git", "ls-files", "-z", "--others", "--ignored",
                              "--exclude-standard", "--directory"], cwd=fixture,
                             capture_output=True, check=True, timeout=60)
    if ignored.stdout:
        raise ValueError("fixture contains ignored untracked paths outside Runtime Snapshot")
    listed = subprocess.run(["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard",
                             "--", ".", ":(exclude).pipeline"], cwd=fixture, capture_output=True,
                            check=True, timeout=300)
    aggregate = hashlib.sha256()
    for raw in sorted(set(part for part in listed.stdout.split(b"\0") if part)):
        rel = raw.decode("utf-8")
        if not safe_rel(rel) or any(part in (".git", ".pipeline") for part in rel.split("/")):
            continue
        path = fixture / rel
        if not path.is_file() or path.is_symlink():
            raise ValueError(f"unsafe captured fixture file: {rel}")
        data = path.read_bytes()
        aggregate.update(f"{len(raw)}\x1f{rel}\x1f{len(data)}\x1e".encode())
        aggregate.update(data)
    return aggregate.hexdigest()


def validate_variant(path: Path) -> dict[str, Any]:
    variant = json.loads(path.read_text())
    if not isinstance(variant, dict) or not isinstance(variant.get("models"), dict):
        raise ValueError("variant requires models")
    if set(variant) - {"id", "models", "base_url", "scout_interval_secs"}:
        raise ValueError("unsupported variant field")
    if "id" in variant and (not isinstance(variant["id"], str) or not variant["id"]):
        raise ValueError("variant id must be a nonempty string")
    for tier in ("strong", "main", "cheap"):
        if not isinstance(variant["models"].get(tier), str) or not variant["models"][tier]:
            raise ValueError(f"variant models.{tier} required")
    if not isinstance(variant.get("base_url", ""), str):
        raise ValueError("variant base_url must be a string")
    interval = variant.get("scout_interval_secs", 3600)
    if type(interval) is not int or interval < 1:
        raise ValueError("scout_interval_secs must be positive")
    validate_base_url(variant.get("base_url", ""))
    if any(key.lower() in ("api_key", "token", "secret", "password") for key in variant):
        raise ValueError("variant must not contain credentials")
    return variant


def prepare(task_id: str, version: str, fixture: Path, prompt: Path, acceptance: Path,
            variant: Path, output: Path, template: Path = DEFAULT_TEMPLATE) -> dict[str, Any]:
    check_task_id(task_id)
    if not version:
        raise ValueError("task version required")
    if output.exists():
        raise ValueError("task definition output already exists")
    fixture, prompt, acceptance, variant, template = [p.resolve(strict=True) for p in
                                                       (fixture, prompt, acceptance, variant, template)]
    spec, _, spec_digest = load_spec(acceptance)
    if json.loads((template / "team.json").read_text()).get("id") != "coding-team":
        raise ValueError("template must define coding-team")
    prompt_bytes = prompt.read_bytes()
    if spec["fixture_digest"] != sha(prompt_bytes):
        raise ValueError("acceptance fixture must match the task prompt bytes")
    validate_variant(variant)
    result = {"schema_version": "1", "task_id": task_id, "version": version,
              "fixture_dir": str(fixture), "fixture_tree_digest": tree_digest(fixture),
              "baseline_digest": baseline_digest(fixture),
              "prompt_path": str(prompt), "prompt_digest": sha(prompt_bytes),
              "acceptance_spec": str(acceptance), "acceptance_spec_digest": spec_digest,
              "suite_digest": spec["suite_digest"], "suite_version": spec["suite_version"],
              "variant_path": str(variant), "variant_digest": sha(variant.read_bytes()),
              "variant_id": validate_variant(variant).get("id", "default"),
              "template_dir": str(template), "template_digest": tree_digest(template)}
    save_json(output, result)
    return result


def source_state() -> dict[str, Any]:
    def git(*args: str) -> str:
        try:
            return subprocess.run(["git", *args], cwd=REPO, capture_output=True,
                                  text=True, check=True, timeout=5).stdout.strip()
        except (OSError, subprocess.SubprocessError):
            return "unavailable"
    status = git("status", "--porcelain", "--untracked-files=normal")
    return {"revision": git("rev-parse", "HEAD"),
            "dirty": None if status == "unavailable" else bool(status),
            "status_digest": sha(status.encode()) if status != "unavailable" else None}


class QueueStore:
    def __init__(self, root: Path):
        self.root = root.resolve()
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        os.chmod(self.root, 0o700)
        self.db = self.root / "queue.sqlite3"
        with self.connect() as conn:
            conn.executescript("""CREATE TABLE IF NOT EXISTS tasks (
                task_id TEXT PRIMARY KEY, state TEXT NOT NULL, definition TEXT NOT NULL,
                attempt_id TEXT, workflow_id TEXT, artifact_dir TEXT, classification TEXT,
                error TEXT, updated_at TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS transitions (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL,
                state TEXT NOT NULL, at TEXT NOT NULL, detail TEXT);""")

    @contextmanager
    def connect(self):
        conn = sqlite3.connect(self.db, timeout=10)
        try:
            yield conn
            conn.commit()
        finally:
            conn.close()

    @contextmanager
    def lock(self):
        with (self.root / "queue.lock").open("a+") as stream:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
            try:
                yield
            finally:
                fcntl.flock(stream, fcntl.LOCK_UN)

    def rows(self) -> list[dict[str, Any]]:
        with self.connect() as conn:
            conn.row_factory = sqlite3.Row
            return [dict(row) for row in conn.execute("SELECT * FROM tasks ORDER BY rowid")]

    def one(self, task_id: str) -> dict[str, Any]:
        row = next((row for row in self.rows() if row["task_id"] == task_id), None)
        if row is None:
            raise ValueError(f"unknown task: {task_id}")
        return row

    def transition(self, task_id: str, state: str, detail: str | None = None, **updates: Any) -> None:
        allowed = {"attempt_id", "workflow_id", "artifact_dir", "classification", "error"}
        if not set(updates) <= allowed:
            raise ValueError("invalid queue update")
        if state in ("blocked", "failed", "interrupted") and detail and "error" not in updates:
            updates["error"] = detail
        with self.connect() as conn:
            columns = ["state=?", "updated_at=?"] + [f"{key}=?" for key in updates]
            args = [state, stamp(), *updates.values(), task_id]
            conn.execute(f"UPDATE tasks SET {', '.join(columns)} WHERE task_id=?", args)
            conn.execute("INSERT INTO transitions(task_id,state,at,detail) VALUES(?,?,?,?)",
                         (task_id, state, stamp(), detail))

    def enqueue(self, path: Path) -> str:
        obj = json.loads(path.read_text())
        task_id = obj["task_id"]
        check_task_id(task_id)
        if any(row["task_id"] == task_id for row in self.rows()):
            raise ValueError("task already queued")
        source = Path(obj["fixture_dir"])
        target = self.root / "tasks" / task_id
        if target.is_symlink():
            raise ValueError("unsafe task package symlink")
        if target.exists():
            pinned = json.loads((target / "definition.json").read_text())
            identity = ("task_id", "version", "fixture_tree_digest", "baseline_digest",
                        "prompt_digest", "acceptance_spec_digest", "variant_digest", "template_digest")
            if any(pinned.get(key) != obj.get(key) for key in identity):
                raise ValueError("existing task package does not match definition")
            if (tree_digest(target / "fixture") != obj["fixture_tree_digest"] or
                    baseline_digest(target / "fixture") != obj["baseline_digest"] or
                    sha((target / "prompt.txt").read_bytes()) != obj["prompt_digest"] or
                    sha((target / "variant.json").read_bytes()) != obj["variant_digest"] or
                    tree_digest(target / "template") != obj["template_digest"]):
                raise ValueError("existing task package is damaged")
            load_spec(target / "acceptance/spec.json", obj["acceptance_spec_digest"])
            with self.connect() as conn:
                conn.execute("INSERT INTO tasks(task_id,state,definition,updated_at) VALUES(?,?,?,?)",
                             (task_id, "queued", str(target / "definition.json"), stamp()))
                conn.execute("INSERT INTO transitions(task_id,state,at) VALUES(?,?,?)", (task_id, "queued", stamp()))
            return task_id
        if target.is_relative_to(source.resolve()) or source.resolve().is_relative_to(target):
            raise ValueError("queue task package must be outside fixture")
        if tree_digest(source) != obj["fixture_tree_digest"] or baseline_digest(source) != obj["baseline_digest"]:
            raise ValueError("fixture changed since preparation")
        prompt = Path(obj["prompt_path"])
        variant = Path(obj["variant_path"])
        template = Path(obj["template_dir"])
        acceptance = Path(obj["acceptance_spec"])
        spec, suite, spec_digest = load_spec(acceptance, obj["acceptance_spec_digest"])
        if (sha(prompt.read_bytes()) != obj["prompt_digest"] or
                sha(variant.read_bytes()) != obj["variant_digest"] or
                tree_digest(template) != obj["template_digest"] or
                spec["suite_digest"] != obj["suite_digest"]):
            raise ValueError("task inputs changed since preparation")
        staging = target.with_name(task_id + ".staging-" + uuid.uuid4().hex)
        staging.mkdir(parents=True, mode=0o700)
        shutil.copytree(source, staging / "fixture")
        shutil.copytree(template, staging / "template")
        (staging / "prompt.txt").write_bytes(prompt.read_bytes())
        (staging / "variant.json").write_bytes(variant.read_bytes())
        (staging / "acceptance").mkdir()
        (staging / "acceptance/spec.json").write_bytes(acceptance.read_bytes())
        suite_target = staging / "acceptance" / spec["suite_path"]
        fixture_target = staging / "acceptance" / spec["fixture_path"]
        suite_target.parent.mkdir(parents=True, exist_ok=True)
        fixture_target.parent.mkdir(parents=True, exist_ok=True)
        suite_target.write_bytes(suite.read_bytes())
        fixture_target.write_bytes((acceptance.parent / spec["fixture_path"]).read_bytes())
        pinned = dict(obj)
        pinned.update(fixture_dir=str(target / "fixture"), prompt_path=str(target / "prompt.txt"),
                      variant_path=str(target / "variant.json"), template_dir=str(target / "template"),
                      acceptance_spec=str(target / "acceptance/spec.json"), source_state=source_state())
        if (tree_digest(staging / "fixture") != obj["fixture_tree_digest"] or
                baseline_digest(staging / "fixture") != obj["baseline_digest"] or
                tree_digest(staging / "template") != obj["template_digest"] or
                sha((staging / "prompt.txt").read_bytes()) != obj["prompt_digest"] or
                sha((staging / "variant.json").read_bytes()) != obj["variant_digest"]):
            raise ValueError("pinned task copy failed verification")
        load_spec(staging / "acceptance/spec.json", obj["acceptance_spec_digest"])
        save_json(staging / "definition.json", pinned)
        os.replace(staging, target)
        with self.connect() as conn:
            conn.execute("INSERT INTO tasks(task_id,state,definition,updated_at) VALUES(?,?,?,?)",
                         (task_id, "queued", str(target / "definition.json"), stamp()))
            conn.execute("INSERT INTO transitions(task_id,state,at) VALUES(?,?,?)", (task_id, "queued", stamp()))
        return task_id


def journal_messages(data_root: Path, team_id: str) -> list[dict[str, Any]]:
    path = data_root / "teams" / team_id / "chat.sqlite3"
    if not path.is_file():
        return []
    try:
        with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as conn:
            return [json.loads(row[0]) for row in conn.execute("SELECT payload FROM chat_messages ORDER BY id")]
    except (sqlite3.Error, ValueError):
        return []


def journal_workflow(data_root: Path, team_id: str, prompt: str) -> str | None:
    matches = {m.get("broadcast_id") for m in journal_messages(data_root, team_id)
               if m.get("from") == "operator" and m.get("to") == "team" and m.get("body") == prompt
               and isinstance(m.get("broadcast_id"), str)}
    return next(iter(matches)) if len(matches) == 1 else None


def journal_terminal(data_root: Path, team_id: str, workflow_id: str) -> dict[str, Any] | None:
    for message in journal_messages(data_root, team_id):
        if message.get("message_id") == f"{workflow_id}:terminal" and message.get("broadcast_id") == workflow_id:
            return {"message_id": message["message_id"], "broadcast_id": workflow_id,
                    "text": message.get("body", "")}
    return None


class OfflineApi:
    def __init__(self, workspace: Path, terminal: dict[str, Any] | None):
        self.workspace, self.terminal = workspace, terminal

    def project_repo(self, project: str) -> str:
        return str(self.workspace)

    def terminal_message(self, project: str, issue: str) -> dict[str, Any] | None:
        return self.terminal if self.terminal and self.terminal["broadcast_id"] == issue else None


class StudioSession:
    def __init__(self, binary: Path, data_root: Path, variant: dict[str, Any], key_file: Path):
        self.binary, self.data_root, self.variant, self.key_file = binary, data_root, variant, key_file
        self.process: subprocess.Popen | None = None
        self.log_stream = None
        self.api: RuntimeApi | None = None
        self.socket: LiveSocket | None = None
        self.reader: threading.Thread | None = None
        self.stop_reader = threading.Event()
        self.events: queue.Queue[tuple[str, Any]] = queue.Queue(maxsize=256)
        self.dropped_events = 0
        self.drop_lock = threading.Lock()
        self.pending_gaps = 0
        self.pending_disconnect = False

    def start(self, deadline: float) -> RuntimeApi:
        directory = self.data_root.lstat()
        if (not stat.S_ISDIR(directory.st_mode) or stat.S_IMODE(directory.st_mode) != 0o700
                or directory.st_uid != os.getuid()):
            raise ValueError("runtime state must be an owned private directory (0700)")
        models = self.variant["models"]
        save_json(self.data_root / "settings.json", {"models": models,
                  "base_url": self.variant.get("base_url", ""),
                  "scout_interval_secs": self.variant.get("scout_interval_secs", 3600)})
        if self.key_file.is_symlink() or not self.key_file.is_file():
            raise ValueError("missing or unsafe provider key file")
        key = self.key_file.read_bytes()
        if not key.strip():
            raise ValueError("empty provider key file")
        secret = self.data_root / "apikey"
        fd = os.open(secret, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "wb") as stream:
            stream.write(key)
        isolated_home = self.data_root / "home"
        isolated_home.mkdir(mode=0o700)
        environment = {key: value for key, value in os.environ.items()
                       if key in ("PATH", "LANG", "LC_ALL", "USER", "TMPDIR")}
        environment.update(STUDIO_DATA_ROOT=str(self.data_root), STUDIO_PORT="0",
                           STUDIO_EVALUATION="1",
                           OPENAI_API_BASE=self.variant.get("base_url", ""),
                           HOME=str(isolated_home), XDG_CONFIG_HOME=str(isolated_home),
                           GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1")
        self.log_stream = (self.data_root / "studio.log").open("ab")
        self.process = subprocess.Popen([str(self.binary)], cwd=REPO, env=environment,
                                        stdout=self.log_stream, stderr=subprocess.STDOUT,
                                        start_new_session=True)
        descriptor = self.data_root / "panel.json"
        until = min(deadline, time.monotonic() + 45)
        while time.monotonic() < until:
            if descriptor.is_file():
                try:
                    self.api = RuntimeApi(descriptor)
                    self.api.get("/api/team/projects", timeout=max(0.1, min(5, deadline - time.monotonic())))
                    return self.api
                except (OSError, ValueError):
                    pass
            if self.process.poll() is not None:
                raise RuntimeError("Studio process exited during startup")
            time.sleep(0.1)
        raise TimeoutError("Studio readiness deadline")

    def ready(self, deadline: float) -> None:
        assert self.api is not None
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("collector readiness deadline")
        self.socket = LiveSocket(self.api, timeout=min(10, remaining))
        first = json.loads(self.socket.receive())
        if first.get("type") != "event_stream_ready":
            raise RuntimeError("collector readiness marker missing")
        self.socket.sock.settimeout(20)

    def collect(self, collector: RunCollector) -> None:
        assert self.socket is not None
        def offer(kind: str, value: Any) -> None:
            try:
                self.events.put_nowait((kind, value))
            except queue.Full:
                with self.drop_lock:
                    self.dropped_events += 1
        def read() -> None:
            ping_sent = None
            while not self.stop_reader.is_set():
                try:
                    event = json.loads(self.socket.receive())
                    if event.get("type") == "event_stream_gap":
                        count = event.get("dropped", 0)
                        offer("gap", count if type(count) is int and count > 0 else 1)
                    else:
                        raw = json.dumps(event, separators=(",", ":"))
                        if len(raw) > 1024 * 1024:
                            with self.drop_lock:
                                self.dropped_events += 1
                        else:
                            offer("event", raw)
                except socket.timeout:
                    if ping_sent is not None and self.socket.last_pong < ping_sent:
                        offer("disconnect", None)
                        return
                    try:
                        ping_sent = time.monotonic()
                        self.socket._send(9, b"")
                    except OSError:
                        offer("disconnect", None)
                        return
                except (OSError, ValueError, ConnectionError):
                    offer("disconnect", None)
                    return
        self.reader = threading.Thread(target=read, daemon=True)
        self.reader.start()

    def drain(self, collector: RunCollector) -> None:
        while True:
            try:
                kind, value = self.events.get_nowait()
            except queue.Empty:
                with self.drop_lock:
                    dropped = self.dropped_events
                    self.dropped_events = 0
                if dropped:
                    if collector.runs:
                        collector.mark_stream_gap(dropped)
                    else:
                        self.pending_gaps += dropped
                return
            if kind == "gap":
                if collector.runs:
                    collector.mark_stream_gap(value)
                else:
                    self.pending_gaps += value
            elif kind == "disconnect":
                if collector.runs:
                    collector.mark_disconnected()
                else:
                    self.pending_disconnect = True
            else:
                collector.process_event(value)
            if collector.runs:
                if self.pending_gaps:
                    collector.mark_stream_gap(self.pending_gaps)
                    self.pending_gaps = 0
                if self.pending_disconnect:
                    collector.mark_disconnected()
                    self.pending_disconnect = False

    def close(self) -> None:
        self.stop_reader.set()
        if self.socket:
            try:
                self.socket.close()
            except OSError:
                pass
        if self.reader:
            self.reader.join(timeout=2)
        if self.process and self.process.poll() is None:
            try:
                os.killpg(self.process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                self.process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(self.process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                self.process.wait()
        if self.log_stream:
            self.log_stream.close()
        secret = self.data_root / "apikey"
        if secret.is_file():
            secret.unlink()


def finish_from_evidence(store: QueueStore, row: dict[str, Any], definition: dict[str, Any],
                         deadline: float | None = None) -> bool:
    attempt_id = row["attempt_id"]
    if not attempt_id:
        return False
    attempt = store.root / "attempts" / attempt_id
    result_paths = sorted((attempt / "evaluation/runs").glob("*/result.json"))
    if len(result_paths) > 1:
        store.transition(row["task_id"], "blocked", "multiple result packages for one attempt",
                         classification="evidence_ambiguous")
        return False
    if result_paths:
        try:
            result = json.loads(result_paths[0].read_text())
            classification = result["outcome"]["category"]
            run_dir = result_paths[0].parent
            record = json.loads((run_dir / "record.json").read_text())
            acceptance_dir = run_dir / record.get("acceptance_dir", "acceptance")
            if deadline is not None and time.monotonic() >= deadline:
                raise TimeoutError("recovery deadline exceeded before re-grading")
            fresh = grade(run_dir / "source",
                          acceptance_dir=acceptance_dir if acceptance_dir.is_dir() else None)
            if canonical(result) != canonical(fresh):
                raise ValueError("saved grading differs from current evidence")
        except TimeoutError:
            # Recovery is bounded: an expired deadline is an interrupted
            # attempt (never a silent retry or invented terminal state).
            store.transition(row["task_id"], "interrupted",
                             "recovery deadline exceeded before re-grading",
                             classification="interrupted")
            return False
        except (OSError, ValueError, KeyError, TypeError):
            store.transition(row["task_id"], "blocked", "saved result is unreadable",
                             classification="evidence_corrupt")
            return False
        queue_state = ("completed" if classification == "completed" else
                       "failed" if classification == "runtime_failed" else "blocked")
        if classification == "completed":
            code_status = result.get("metrics", {}).get("code_correctness", {}).get("status")
            if code_status != "measured":
                queue_state = "blocked"
                classification = ("evidence_corrupt" if code_status == "measurement_error"
                                  else "acceptance_unavailable")
        store.transition(row["task_id"], queue_state,
                         classification=classification, artifact_dir=str(result_paths[0].parent))
        return True
    team_id = "eval-" + attempt_id.replace("-", "")[:24]
    runtime = store.root / "runtime-state" / attempt_id
    prompt = Path(definition["prompt_path"]).read_text()
    workflow = row["workflow_id"] or journal_workflow(runtime, team_id, prompt)
    if not workflow:
        return False
    terminal = journal_terminal(runtime, team_id, workflow)
    if not terminal:
        return False
    workspace = attempt / "workspace"
    collector = RunCollector(attempt / "evaluation", OfflineApi(workspace, terminal),
                             {"projects": {team_id: collector_metadata(definition)}})
    record = collector.runs.get((team_id, workflow))
    if record is None:
        record = collector._new_run(team_id, workflow, missed_start=True)
    if record:
        collector._gap(record, "offline recovery; live event stream unavailable")
        collector.reconcile()
    row = store.one(row["task_id"])
    if not row["workflow_id"]:
        store.transition(row["task_id"], "grading", workflow_id=workflow)
    if list((attempt / "evaluation/runs").glob("*/result.json")):
        return finish_from_evidence(store, store.one(row["task_id"]), definition, deadline=deadline)
    return False


def collector_metadata(definition: dict[str, Any]) -> dict[str, Any]:
    spec = json.loads(Path(definition["acceptance_spec"]).read_text())
    return {"fixture_id": spec["fixture_id"], "fixture_version": spec["fixture_version"],
            "fixture_digest": spec["fixture_digest"], "baseline_digest": definition["baseline_digest"],
            "suite_digest": spec["suite_digest"], "variant_id": definition["variant_id"],
            "acceptance_spec": definition["acceptance_spec"],
            "acceptance_spec_digest": definition["acceptance_spec_digest"]}


def provider_identity(variant: dict[str, Any]) -> str:
    return urlsplit(variant["base_url"]).hostname or "api.openai.com"


def redact_secrets(value: Any) -> Any:
    if isinstance(value, dict):
        return {key: "[REDACTED]" if any(word in key.lower() for word in
                ("api_key", "token", "secret", "password", "authorization"))
                else redact_secrets(item) for key, item in value.items()}
    if isinstance(value, list):
        return [redact_secrets(item) for item in value]
    return value


def execute_one(store: QueueStore, row: dict[str, Any], *, binary: Path, key_file: Path,
                deadline: float, allow_models: set[str] | None = None,
                allow_providers: set[str] | None = None,
                session_factory: Any = StudioSession) -> None:
    definition = json.loads(Path(row["definition"]).read_text())
    variant = validate_variant(Path(definition["variant_path"]))
    models = set(variant["models"].values())
    if allow_models is not None and not models <= allow_models:
        store.transition(row["task_id"], "blocked", "model outside allowlist",
                         classification="configuration_blocked")
        return
    provider = provider_identity(variant)
    if allow_providers is not None and provider not in allow_providers:
        store.transition(row["task_id"], "blocked", "provider outside allowlist",
                         classification="configuration_blocked")
        return
    if time.monotonic() >= deadline:
        return
    if variant.get("scout_interval_secs", 3600) <= deadline - time.monotonic():
        store.transition(row["task_id"], "blocked", "scout interval must exceed attempt deadline",
                         classification="configuration_blocked")
        return
    attempt_id = str(uuid.uuid4())
    team_id = "eval-" + attempt_id.replace("-", "")[:24]
    attempt = store.root / "attempts" / attempt_id
    workspace = attempt / "workspace"
    runtime = store.root / "runtime-state" / attempt_id
    store.transition(row["task_id"], "preparing", attempt_id=attempt_id,
                     artifact_dir=str(attempt))
    session = None
    try:
        if tree_digest(Path(definition["fixture_dir"])) != definition["fixture_tree_digest"]:
            raise ValueError("pinned fixture changed")
        if baseline_digest(Path(definition["fixture_dir"])) != definition["baseline_digest"]:
            raise ValueError("pinned baseline changed")
        if sha(Path(definition["prompt_path"]).read_bytes()) != definition["prompt_digest"]:
            raise ValueError("pinned prompt changed")
        if sha(Path(definition["variant_path"]).read_bytes()) != definition["variant_digest"]:
            raise ValueError("pinned variant changed")
        acceptance_spec, _, _ = load_spec(Path(definition["acceptance_spec"]),
                                          definition["acceptance_spec_digest"])
        if tree_digest(Path(definition["template_dir"])) != definition["template_digest"]:
            raise ValueError("pinned template changed")
        attempt.mkdir(parents=True, mode=0o700)
        shutil.copytree(definition["fixture_dir"], workspace)
        runtime.mkdir(parents=True, mode=0o700)
        os.chmod(runtime, 0o700)
        shutil.copytree(definition["template_dir"], runtime / "templates/coding-team")
        prompt = Path(definition["prompt_path"]).read_text()
        (attempt / "prompt.txt").write_text(prompt)
        metadata = {"attempt_id": attempt_id, "team_id": team_id, "task_id": row["task_id"],
                    "task_version": definition["version"], "fixture_tree_digest": definition["fixture_tree_digest"],
                    "fixture_id": acceptance_spec["fixture_id"],
                    "fixture_version": acceptance_spec["fixture_version"],
                    "fixture_digest": acceptance_spec["fixture_digest"],
                    "pinned_task_definition": row["definition"],
                    "baseline_digest": definition["baseline_digest"], "prompt_digest": definition["prompt_digest"],
                    "suite_digest": definition["suite_digest"],
                    "suite_version": definition["suite_version"],
                    "acceptance_spec_digest": definition["acceptance_spec_digest"],
                    "variant_id": definition["variant_id"],
                    "variant_digest": definition["variant_digest"], "template_digest": definition["template_digest"],
                    "template_prompt_digests": template_prompt_digests(Path(definition["template_dir"])),
                    "source_at_enqueue": definition.get("source_state"), "source_at_run": source_state(),
                    "runtime_binary_digest": sha(binary.read_bytes()) if binary.is_file() else None,
                    "models": variant["models"], "base_url": variant.get("base_url", ""),
                    "provider": provider,
                    "scout_interval_secs": variant.get("scout_interval_secs", 3600),
                    "inference_parameters": {"provider_default": "OpenAI-compatible defaults",
                                             "per_request_overrides": "not exposed by Studio settings"},
                    "grader_version": GRADER_VERSION, "acceptance_runner_version": RUNNER_VERSION}
        save_json(attempt / "metadata.json", metadata)
        session = session_factory(binary, runtime, variant, key_file)
        api = session.start(deadline)
        # Runtime must enforce evaluation policy, not just report an editable
        # team field. Refuse older/development runtimes before registration.
        startup = api.get("/api/team/startup",
                          timeout=max(0.1, min(15, deadline - time.monotonic())))
        if startup.get("execution_mode") != "evaluation":
            raise ConfinementNotVerified("Runtime is not enforcing evaluation execution policy")
        grant = api.post("/api/team/fs/grants", {"path": str(workspace), "label": team_id},
                         timeout=max(0.1, min(15, deadline - time.monotonic())))
        if not isinstance(grant.get("id"), str):
            raise RuntimeError("workspace grant response missing id")
        registered = api.post("/api/team/projects", {"id": team_id, "name": team_id,
                                                      "template_id": "coding-team", "repo_path": str(workspace)},
                              timeout=max(0.1, min(15, deadline - time.monotonic())))
        if not registered.get("ok"):
            raise RuntimeError("production team registration was not accepted")
        team_spec = runtime / "teams" / team_id / "team.json"
        if not team_spec.is_file():
            raise RuntimeError("effective team spec missing")
        effective = redact_secrets(json.loads(team_spec.read_text()))
        sandbox_mode = effective.get("sandbox_mode")
        if sandbox_mode != "bwrap":
            # Fail closed BEFORE any evaluated agent execution: the effective
            # runtime team must be running under the bubblewrap sandbox.
            # os-fallback, missing, unknown or future confinement states are
            # all rejected; the chat submission below never happens.
            raise ConfinementNotVerified(
                f"effective sandbox_mode={sandbox_mode!r}; "
                "evaluation requires 'bwrap' (refusing to submit work to an "
                "unconfined or unverifiable execution environment)")
        member_config = []
        for member in effective.get("members", []):
            override = member.get("provider_override") or {}
            model = override.get("model") or variant["models"].get(member.get("model"), member.get("model"))
            base_url = override.get("base_url") or variant.get("base_url", "")
            validate_base_url(base_url)
            member_provider = provider_identity({"base_url": base_url})
            if allow_models is not None and model not in allow_models:
                raise ValueError(f"effective model outside allowlist: {member.get('id')}")
            if allow_providers is not None and member_provider not in allow_providers:
                raise ValueError(f"effective provider outside allowlist: {member.get('id')}")
            member_config.append({"member": member.get("id"), "model": model,
                                  "provider": member_provider})
        save_json(attempt / "effective-team.json", effective)
        metadata["effective_members"] = member_config
        metadata["effective_team_digest"] = sha(canonical(effective))
        comparable_team = {key: value for key, value in effective.items()
                           if key not in ("id", "name", "repo_path")}
        metadata["effective_configuration_digest"] = sha(canonical(comparable_team))
        metadata["configuration_fingerprint"] = sha(canonical({"models": variant["models"],
            "base_url": variant.get("base_url", ""), "template": definition["template_digest"],
            "scout_interval_secs": variant.get("scout_interval_secs", 3600),
            "runtime_binary": metadata["runtime_binary_digest"],
            "effective_team": metadata["effective_configuration_digest"]}))
        save_json(attempt / "metadata.json", metadata)
        collector = RunCollector(attempt / "evaluation", api,
                                 {"projects": {team_id: collector_metadata(definition)}},
                                 deadline=deadline)
        session.ready(deadline)
        session.collect(collector)
        store.transition(row["task_id"], "running", "submission may have been accepted",
                         classification="submission_uncertain")
        try:
            response = api.post("/api/team/chat", {"project": team_id, "target": "team", "text": prompt},
                                timeout=max(0.1, min(15, deadline - time.monotonic())))
            workflow = response.get("workflow_id")
            if not response.get("ok") or not isinstance(workflow, str) or not workflow:
                raise RuntimeError("chat response lacks workflow_id")
        except Exception:
            # The server can accept work before the HTTP reply is lost. Never retry POST.
            workflow = None
            for _ in range(10):
                session.drain(collector)
                observed = [issue for (project, issue) in collector.runs if project == team_id]
                workflow = observed[0] if len(observed) == 1 else journal_workflow(runtime, team_id, prompt)
                if workflow:
                    break
                if time.monotonic() >= deadline:
                    break
                time.sleep(min(0.5, max(0, deadline - time.monotonic())))
            if not workflow:
                store.transition(row["task_id"], "blocked", "submission outcome uncertain; investigate runtime journal",
                                 classification="submission_uncertain")
                return
        store.transition(row["task_id"], "collecting", workflow_id=workflow,
                         classification=None)
        metadata["workflow_id"] = workflow
        save_json(attempt / "metadata.json", metadata)
        waiting_since = time.monotonic()
        while time.monotonic() < deadline:
            session.drain(collector)
            if (team_id, workflow) not in collector.runs and time.monotonic() - waiting_since > 2:
                collector._new_run(team_id, workflow, missed_start=True)
            collector.reconcile()
            record = collector.runs.get((team_id, workflow))
            if record and record["state"] == "graded":
                store.transition(row["task_id"], "grading")
                if finish_from_evidence(store, store.one(row["task_id"]), definition, deadline=deadline):
                    return
            time.sleep(0.2)
        store.transition(row["task_id"], "blocked", "attempt deadline; workflow may still need investigation",
                         classification="timed_out")
    except ConfinementNotVerified as exc:
        store.transition(row["task_id"], "blocked", str(exc),
                         classification="confinement_unverified")
    except TimeoutError as exc:
        store.transition(row["task_id"], "blocked", f"{type(exc).__name__}: {exc}",
                         classification="timed_out")
    except Exception as exc:
        store.transition(row["task_id"], "failed", f"{type(exc).__name__}: {exc}",
                         classification="infrastructure_failed")
    finally:
        if session:
            session.close()


def run_queue(store: QueueStore, *, binary: Path, key_file: Path, max_attempts: int,
              max_duration: int, attempt_timeout: int, allow_models: set[str] | None = None,
              allow_providers: set[str] | None = None,
              session_factory: Any = StudioSession) -> int:
    if max_attempts < 1 or max_duration < 1 or attempt_timeout < 1:
        raise ValueError("limits must be positive")
    with store.lock():
        # Recovery (re-grading persisted evidence) is itself bounded: without
        # a recovery deadline a slow grader could consume the entire
        # max_duration before any queued task is attempted.
        recovery_deadline = time.monotonic() + min(max_duration, attempt_timeout)
        for row in store.rows():
            if row["state"] in ("preparing", "running", "collecting", "grading", "blocked", "failed", "interrupted") and row["attempt_id"]:
                try:
                    definition = json.loads(Path(row["definition"]).read_text())
                    if (not finish_from_evidence(store, row, definition, deadline=recovery_deadline) and
                            store.one(row["task_id"])["state"] not in FINAL):
                        store.transition(row["task_id"], "interrupted", "no terminal evidence; no automatic resubmission",
                                         classification="interrupted")
                except (OSError, ValueError, KeyError, TypeError) as exc:
                    store.transition(row["task_id"], "blocked", f"recovery evidence error: {exc}",
                                     classification="evidence_corrupt")
        overall = time.monotonic() + max_duration
        processed = 0
        infrastructure_failed = False
        for row in store.rows():
            if row["state"] != "queued" or processed >= max_attempts or time.monotonic() >= overall:
                continue
            processed += 1
            execute_one(store, row, binary=binary, key_file=key_file,
                        deadline=min(overall, time.monotonic() + attempt_timeout),
                        allow_models=allow_models, allow_providers=allow_providers,
                        session_factory=session_factory)
            if store.one(row["task_id"])["classification"] == "infrastructure_failed":
                infrastructure_failed = True
        return 1 if infrastructure_failed else 0


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prep = commands.add_parser("prepare")
    for key in ("task-id", "version", "fixture", "prompt", "acceptance-spec", "variant", "output"):
        prep.add_argument("--" + key, required=True)
    prep.add_argument("--template-dir", default=str(DEFAULT_TEMPLATE))
    enq = commands.add_parser("enqueue")
    enq.add_argument("--queue", required=True, type=Path)
    enq.add_argument("definition", type=Path)
    listing = commands.add_parser("list")
    listing.add_argument("--queue", required=True, type=Path)
    start = commands.add_parser("start")
    start.add_argument("--queue", required=True, type=Path)
    start.add_argument("--studio-bin", type=Path, default=REPO / "target/release/agent-studio")
    start.add_argument("--key-file", type=Path, required=True)
    start.add_argument("--max-attempts", type=int, default=1)
    start.add_argument("--max-duration", type=int, default=3600)
    start.add_argument("--attempt-timeout", type=int, default=1800)
    start.add_argument("--allow-model", action="append")
    start.add_argument("--allow-provider", action="append", help="allowed provider hostname")
    args = parser.parse_args()
    if args.command == "prepare":
        result = prepare(args.task_id, args.version, Path(args.fixture), Path(args.prompt),
                         Path(args.acceptance_spec), Path(args.variant), Path(args.output),
                         Path(args.template_dir))
        print(json.dumps({"task_id": result["task_id"], "baseline_digest": result["baseline_digest"]}))
    elif args.command == "enqueue":
        print(QueueStore(args.queue).enqueue(args.definition))
    elif args.command == "list":
        summaries = []
        for row in QueueStore(args.queue).rows():
            definition = json.loads(Path(row["definition"]).read_text())
            summary = {key: value for key, value in row.items() if key != "definition"}
            summary.update(version=definition["version"], variant_id=definition["variant_id"],
                           baseline_digest=definition["baseline_digest"],
                           prompt_digest=definition["prompt_digest"],
                           suite_digest=definition["suite_digest"])
            summaries.append(summary)
        print(json.dumps(summaries, indent=2))
    else:
        sys.exit(run_queue(QueueStore(args.queue), binary=args.studio_bin.resolve(strict=True),
                           key_file=args.key_file.resolve(strict=True), max_attempts=args.max_attempts,
                           max_duration=args.max_duration, attempt_timeout=args.attempt_timeout,
                           allow_models=set(args.allow_model) if args.allow_model else None,
                           allow_providers=set(args.allow_provider) if args.allow_provider else None))


if __name__ == "__main__":
    main()
