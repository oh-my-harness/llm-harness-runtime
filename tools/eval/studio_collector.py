"""采集 Studio 手动 Coding Team 工作流并调用 P1 离线评分器。"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import stat
import struct
import time
from typing import Any
from urllib.parse import parse_qs, quote, urlparse
from urllib.request import ProxyHandler, Request, build_opener

from offline_grader import snapshot_paths, verify_snapshot, write_report
from acceptance_runner import load_spec, run_acceptance

TERMINAL_TYPES = {"pipeline_completed", "pipeline_escalated", "pipeline_failed"}
CONTROLLED_FIELDS = ("fixture_id", "fixture_version", "fixture_digest", "baseline_digest",
                     "suite_digest", "variant_id")


def save_json(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temp = path.with_suffix(path.suffix + ".tmp")
    temp.write_text(json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False) + "\n")
    os.replace(temp, path)


class RuntimeApi:
    def __init__(self, panel_descriptor: Path):
        self.data_root = panel_descriptor.parent
        descriptor = json.loads(panel_descriptor.read_text())
        parsed = urlparse(descriptor["url"])
        if parsed.scheme != "http" or parsed.hostname not in ("localhost", "127.0.0.1"):
            raise ValueError("collector requires a local Studio panel descriptor")
        token = parse_qs(parsed.fragment).get("token", [None])[0]
        if not token:
            raise ValueError("panel descriptor has no auth token")
        self.host = parsed.hostname
        self.port = parsed.port or 80
        self.token = token
        self.opener = build_opener(ProxyHandler({}))

    def get(self, path: str, timeout: float = 5) -> dict[str, Any]:
        request = Request(f"http://{self.host}:{self.port}{path}",
                          headers={"Authorization": f"Bearer {self.token}"})
        with self.opener.open(request, timeout=timeout) as response:
            return json.load(response)

    def post(self, path: str, payload: dict[str, Any], timeout: float = 15) -> dict[str, Any]:
        request = Request(f"http://{self.host}:{self.port}{path}",
                          data=json.dumps(payload).encode("utf-8"), method="POST",
                          headers={"Authorization": f"Bearer {self.token}",
                                   "Content-Type": "application/json"})
        with self.opener.open(request, timeout=timeout) as response:
            return json.load(response)

    def project_repo(self, project: str) -> str | None:
        for entry in self.get("/api/team/projects")["projects"]:
            if entry["id"] == project:
                return entry.get("repo_path") or str(self.data_root / "teams" / project)
        return None

    def terminal_message(self, project: str, issue_id: str) -> dict[str, Any] | None:
        # 使用现有持久聊天分页；避免只取最后 50/200 条漏掉重启前的结果。
        before = None
        for _ in range(100):
            path = f"/api/team/chat?project={quote(project)}&agent=team&limit=200"
            if before is not None:
                path += f"&before={before}"
            rows = self.get(path).get("messages", [])
            for row in rows:
                if row.get("message_id") == f"{issue_id}:terminal" and row.get("broadcast_id") == issue_id:
                    return row
            sequences = [row["sequence"] for row in rows if isinstance(row.get("sequence"), int)]
            if len(rows) < 200 or not sequences:
                return None
            before = min(sequences)
        return None


class RunCollector:
    """可用合成事件驱动的采集核心；只有 RuntimeApi 负责网络。"""

    def __init__(self, output: Path, api: Any, metadata: dict[str, Any] | None = None,
                 deadline: float | None = None):
        self.output = output.resolve()
        self.output.mkdir(parents=True, exist_ok=True, mode=0o700)
        if os.name == "posix" and stat.S_IMODE(self.output.stat().st_mode) & 0o077:
            raise ValueError("collector output directory must be private (mode 0700)")
        self.api = api
        self.deadline = deadline
        self.metadata = metadata or {}
        self.runs: dict[tuple[str, str], dict[str, Any]] = {}
        self.ignored: set[tuple[str, str]] = set()
        self.pending: dict[tuple[str, str], list[str]] = {}
        for record_path in sorted(self.output.glob("runs/*/record.json")):
            record = json.loads(record_path.read_text())
            record.setdefault("terminal_chat_seen", False)
            record.setdefault("comparability_issues", [])
            key = (record["project"], record["workflow_id"])
            self.runs[key] = record
            if record["state"] != "graded":
                self._gap(record, "collector restarted before grading")

    def _directory(self, record: dict[str, Any]) -> Path:
        key = f"{record['project']}\0{record['workflow_id']}".encode()
        return self.output / "runs" / hashlib.sha256(key).hexdigest()[:24]

    def _save(self, record: dict[str, Any]) -> None:
        save_json(self._directory(record) / "record.json", record)

    def _gap(self, record: dict[str, Any], reason: str) -> None:
        if reason not in record["collection_gaps"]:
            record["collection_gaps"].append(reason)
            self._save(record)

    def mark_disconnected(self) -> None:
        for record in self.runs.values():
            if record["state"] != "graded":
                self._gap(record, "event socket disconnected; intermediate events may be lost")

    def mark_stream_gap(self, count: int) -> None:
        for record in self.runs.values():
            if record["state"] != "graded":
                self._gap(record, f"Runtime event channel dropped {count} events")

    def _new_run(self, project: str, issue_id: str, *, missed_start: bool = False) -> dict[str, Any] | None:
        lookup_failed = False
        try:
            repo = self.api.project_repo(project)
        except Exception:
            repo = None
            lookup_failed = True
        if repo and self.output.is_relative_to(Path(repo).resolve()):
            self.ignored.add((project, issue_id))
            print("Collector output is inside a Coding Team workspace; run skipped", flush=True)
            return None
        declared = self.metadata.get("projects", {}).get(project, {})
        if not isinstance(declared, dict):
            declared = {}
        declared = {field: declared[field] for field in CONTROLLED_FIELDS if field in declared}
        association = self.metadata.get("projects", {}).get(project, {})
        if not isinstance(association, dict):
            association = {}
        for field in ("acceptance_spec", "acceptance_spec_digest"):
            if isinstance(association.get(field), str):
                declared[field] = association[field]
        record = {"project": project, "workflow_id": issue_id, "repo_path": repo,
                  "state": "collecting", "mode": "ordinary", "evaluation_metadata": declared,
                  "terminal_seen": False, "terminal_chat_seen": False,
                  "collection_gaps": [], "comparability_issues": [], "snapshot_sources": [],
                  "grade_error": None}
        self.runs[(project, issue_id)] = record
        self._save(record)
        if lookup_failed:
            self._gap(record, "project workspace lookup failed; snapshots may be unavailable")
        if missed_start:
            self._gap(record, "pipeline_started was not observed")
        return record

    def _append(self, record: dict[str, Any], raw: str) -> None:
        path = self._directory(record) / "source/evidence/events.jsonl"
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("ab") as stream:
            stream.write(raw.encode("utf-8") + b"\n")
            stream.flush()
            os.fsync(stream.fileno())

    def process_event(self, raw: str) -> None:
        event = json.loads(raw)
        if not isinstance(event, dict) or event.get("type") == "event_stream_ready":
            return
        project = event.get("project")
        if not isinstance(project, str):
            return
        issue = event.get("issue_id")
        if event.get("type") == "team_message":
            issue = event.get("broadcast_id")
        if event.get("type") == "agent_error" and not issue:
            active = [r for r in self.runs.values() if r["project"] == project and r["state"] != "graded"]
            if len(active) == 1:
                issue = active[0]["workflow_id"]
        if not isinstance(issue, str):
            return
        key = (project, issue)
        if key in self.ignored:
            return
        record = self.runs.get(key)
        kind = event.get("type")
        if record is None:
            if kind == "team_message":
                self.pending.setdefault(key, []).append(raw)
                if len(self.pending) > 1000:
                    self.pending.pop(next(iter(self.pending)))
                return
            if not isinstance(kind, str) or not kind.startswith("pipeline_"):
                return
            record = self._new_run(project, issue, missed_start=kind != "pipeline_started")
            if record is None:
                self.pending.pop(key, None)
                return
            for earlier in self.pending.pop(key, []):
                self._append(record, earlier)
        if record["state"] == "graded":
            return
        self._append(record, raw)
        if kind in TERMINAL_TYPES:
            record["terminal_seen"] = True
            record["state"] = "terminal_pending"
            self._save(record)
            self._try_grade(record)
        elif kind == "team_message" and event.get("message_id") == f"{issue}:terminal":
            record["terminal_chat_seen"] = True
            self._save(record)
            self._try_grade(record)

    def _copy_snapshots(self, record: dict[str, Any]) -> None:
        repo_text = record.get("repo_path")
        if not repo_text:
            return
        repo = Path(repo_text)
        for manifest in sorted(repo.glob(".pipeline/snapshots/*/r*/snapshot.json")):
            try:
                data = json.loads(manifest.read_text())
            except (OSError, ValueError):
                continue
            if data.get("issue_id") != record["workflow_id"]:
                continue
            source = manifest.parent
            target = self._directory(record) / "source/workspace/.pipeline/snapshots" / source.parent.name / source.name
            # Product Snapshot 不应包含符号链接；拒绝跟随异常路径。
            if (any(path.is_symlink() for path in (repo / ".pipeline", source.parent.parent,
                                                  source.parent, source, manifest)) or
                    not manifest.resolve().is_relative_to(repo.resolve()) or
                    any(path.is_symlink() for path in source.rglob("*"))):
                self._gap(record, f"snapshot contains symlink: {source}")
                continue
            if not target.exists():
                target.parent.mkdir(parents=True, exist_ok=True)
                temporary = target.with_name(target.name + ".copying")
                if temporary.exists():
                    shutil.rmtree(temporary)
                shutil.copytree(source, temporary)
                os.replace(temporary, target)
            origin = {"manifest_path": str(manifest),
                      "manifest_sha256": hashlib.sha256(manifest.read_bytes()).hexdigest()}
            if origin not in record["snapshot_sources"]:
                record["snapshot_sources"].append(origin)
        self._save(record)

    def _set_mode(self, record: dict[str, Any]) -> None:
        declared = record["evaluation_metadata"]
        if not all(isinstance(declared.get(field), str) and declared[field] for field in CONTROLLED_FIELDS):
            return
        for manifest in (self._directory(record) / "source/workspace/.pipeline/snapshots").glob("*/r0/snapshot.json"):
            try:
                data = json.loads(manifest.read_text())
            except (OSError, ValueError):
                continue
            verified = verify_snapshot(manifest, self._directory(record) / "source")
            if (verified["status"] == "valid" and data.get("issue_id") == record["workflow_id"]
                    and data.get("content_hash") == declared["baseline_digest"]):
                record["mode"] = "controlled_metadata_only"
                return
        reason = "declared baseline digest does not match a verified captured r0"
        if reason not in record["comparability_issues"]:
            record["comparability_issues"].append(reason)

    def _try_grade(self, record: dict[str, Any]) -> None:
        if record["state"] == "graded":
            return
        if not record["terminal_chat_seen"] and not record["collection_gaps"]:
            return
        try:
            terminal = self.api.terminal_message(record["project"], record["workflow_id"])
        except Exception:
            return  # Studio may still be writing or restarting; reconcile retries.
        if terminal is None:
            return
        record["terminal_seen"] = True
        terminal_text = terminal.get("text", "")
        record["terminal_message"] = {
            "message_id": terminal.get("message_id", f"{record['workflow_id']}:terminal"),
            "broadcast_id": terminal.get("broadcast_id", record["workflow_id"]),
            "text_sha256": hashlib.sha256(terminal_text.encode()).hexdigest(),
        }
        run_dir = self._directory(record)
        source = run_dir / "source"
        evidence = source / "evidence"
        evidence.mkdir(parents=True, exist_ok=True)
        terminal_path = evidence / "human-review-package.md"
        if not terminal_path.exists():
            terminal_path.write_text(terminal_text)
        elif terminal_path.read_text() != terminal_text:
            self._gap(record, "persisted terminal message changed after capture")
        try:
            self._copy_snapshots(record)
            self._set_mode(record)
            acceptance_dir = run_dir / record.get("acceptance_dir", "acceptance")
            declared = record["evaluation_metadata"]
            if declared.get("acceptance_spec") and declared.get("acceptance_spec_digest"):
                try:
                    spec, _, _ = load_spec(Path(declared["acceptance_spec"]), declared["acceptance_spec_digest"])
                    if (record["mode"] != "controlled_metadata_only" or
                            spec["fixture_id"] != declared["fixture_id"] or
                            spec["fixture_version"] != declared["fixture_version"] or
                            spec["fixture_digest"] != declared["fixture_digest"] or
                            spec["suite_digest"] != declared["suite_digest"]):
                        raise ValueError("acceptance suite does not match controlled fixture metadata")
                    expected = set()
                    for manifest in snapshot_paths(source):
                        data = json.loads(manifest.read_text())
                        if data.get("issue_id") == record["workflow_id"] and data.get("round") in (1, 2):
                            expected.add(f"r{data['round']}.json")
                    if acceptance_dir.exists() and expected and not all(
                            (acceptance_dir / name).is_file() for name in expected):
                        number = 1
                        while (run_dir / f"acceptance-recovery-{number}").exists():
                            number += 1
                        acceptance_dir = run_dir / f"acceptance-recovery-{number}"
                    if not acceptance_dir.exists():
                        if (self.deadline is not None and
                                time.monotonic() + (spec.get("timeout_seconds", 10) + 2) *
                                max(1, len(expected)) > self.deadline):
                            # Every r1/r2 candidate snapshot runs the pinned
                            # suite; the queue deadline must cover all rounds,
                            # not just one, or later rounds would be cut off.
                            raise ValueError("insufficient queue deadline for acceptance suite")
                        run_acceptance(source, declared["acceptance_spec"], acceptance_dir,
                                       issue_id=record["workflow_id"],
                                       expected_spec_digest=declared["acceptance_spec_digest"])
                    record["acceptance_dir"] = acceptance_dir.name
                except (OSError, ValueError) as exc:
                    reason = f"acceptance association unavailable: {exc}"
                    if reason not in record["comparability_issues"]:
                        record["comparability_issues"].append(reason)
            write_report(source, run_dir, acceptance_dir=acceptance_dir if acceptance_dir.exists() else None)
        except Exception as exc:
            record["state"] = "grade_failed"
            record["grade_error"] = f"{type(exc).__name__}: {exc}"
            self._save(record)
            return
        record["state"] = "graded"
        record["grade_error"] = None
        self._save(record)

    def reconcile(self) -> None:
        for record in self.runs.values():
            if record["state"] != "graded":
                self._try_grade(record)


class LiveSocket:
    """只读、仅本机的最小 RFC 6455 文本客户端。"""

    def __init__(self, api: RuntimeApi, timeout: float | None = None):
        self.sock = socket.create_connection((api.host, api.port), timeout=min(10, timeout) if timeout else 10)
        self.sock.settimeout(timeout if timeout else 20)
        key = base64.b64encode(os.urandom(16)).decode()
        request = (f"GET /api/team/events?live_only=true HTTP/1.1\r\n"
                   f"Host: {api.host}:{api.port}\r\nUpgrade: websocket\r\n"
                   f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
                   "Sec-WebSocket-Version: 13\r\n"
                   f"Origin: http://{api.host}:{api.port}\r\n"
                   f"Authorization: Bearer {api.token}\r\n\r\n")
        self.sock.sendall(request.encode())
        response = bytearray()
        while b"\r\n\r\n" not in response and len(response) < 16384:
            response.extend(self.sock.recv(1))
        if not response.startswith(b"HTTP/1.1 101"):
            self.sock.close()
            raise ConnectionError("Studio event WebSocket handshake failed")
        expected = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest())
        headers = {}
        for line in bytes(response).split(b"\r\n")[1:]:
            if b":" in line:
                name, value = line.split(b":", 1)
                headers[name.lower()] = value.strip()
        if headers.get(b"sec-websocket-accept") != expected:
            self.sock.close()
            raise ConnectionError("invalid WebSocket accept header")
        self.last_pong = time.monotonic()

    def _read_exact(self, length: int) -> bytes:
        data = bytearray()
        while len(data) < length:
            chunk = self.sock.recv(length - len(data))
            if not chunk:
                raise ConnectionError("event WebSocket closed")
            data.extend(chunk)
        return bytes(data)

    def _send(self, opcode: int, payload: bytes) -> None:
        mask = os.urandom(4)
        length = len(payload)
        header = bytes([0x80 | opcode, 0x80 | length]) if length < 126 else bytes([0x80 | opcode, 0x80 | 126]) + struct.pack("!H", length)
        self.sock.sendall(header + mask + bytes(byte ^ mask[i % 4] for i, byte in enumerate(payload)))

    def receive(self) -> str:
        parts = bytearray()
        while True:
            head = self._read_exact(2)
            opcode = head[0] & 15
            length = head[1] & 127
            if length == 126:
                length = struct.unpack("!H", self._read_exact(2))[0]
            elif length == 127:
                length = struct.unpack("!Q", self._read_exact(8))[0]
            if length > 16 * 1024 * 1024:
                raise ConnectionError("event frame too large")
            payload = self._read_exact(length)
            if opcode == 8:
                raise ConnectionError("event WebSocket closed")
            if opcode == 9:
                self._send(10, payload)
                continue
            if opcode == 10:
                self.last_pong = time.monotonic()
                continue
            if opcode in (1, 0):
                parts.extend(payload)
                if head[0] & 0x80:
                    return parts.decode("utf-8")

    def close(self) -> None:
        self.sock.close()


def run_forever(api: RuntimeApi, collector: RunCollector) -> None:
    while True:
        connection = None
        try:
            connection = LiveSocket(api)
            first = json.loads(connection.receive())
            if first.get("type") != "event_stream_ready":
                raise ConnectionError("collector readiness marker missing")
            print("Collector READY; Studio prompts can now be submitted", flush=True)
            collector.reconcile()
            ping_sent = None
            while True:
                try:
                    raw = connection.receive()
                    event = json.loads(raw)
                    if event.get("type") == "event_stream_gap":
                        collector.mark_stream_gap(event.get("dropped", 0))
                    else:
                        collector.process_event(raw)
                except socket.timeout:
                    collector.reconcile()
                    if ping_sent is not None and connection.last_pong < ping_sent:
                        raise ConnectionError("event WebSocket heartbeat unanswered")
                    ping_sent = time.monotonic()
                    connection._send(9, b"")
        except (OSError, ConnectionError, ValueError, json.JSONDecodeError) as exc:
            collector.mark_disconnected()
            print(f"Collector connection interrupted: {type(exc).__name__}; retrying", flush=True)
            time.sleep(2)
        finally:
            if connection is not None:
                connection.close()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data-root", type=Path, required=True, help="Studio data root containing panel.json")
    parser.add_argument("--output", type=Path, required=True, help="new collector-owned evaluation root")
    parser.add_argument("--metadata-file", type=Path, help="optional project evaluation identities")
    args = parser.parse_args()
    os.umask(0o077)
    metadata = json.loads(args.metadata_file.read_text()) if args.metadata_file else None
    api = RuntimeApi(args.data_root / "panel.json")
    output = args.output.resolve()
    for project in api.get("/api/team/projects").get("projects", []):
        repo = project.get("repo_path") or str(api.data_root / "teams" / project["id"])
        if output.is_relative_to(Path(repo).resolve()):
            parser.error("--output must be outside every registered Coding Team workspace")
    collector = RunCollector(args.output, api, metadata)
    try:
        run_forever(api, collector)
    except KeyboardInterrupt:
        collector.mark_disconnected()


if __name__ == "__main__":
    main()
