"""Run a fixed Python unittest suite against verified Runtime snapshots."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import resource
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from typing import Any

from offline_grader import safe_rel, snapshot_paths, verify_snapshot

RUNNER_VERSION = "p2.5.0"
MAX_LOG_BYTES = 1024 * 1024


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def load_spec(path: Path, expected_digest: str | None = None) -> tuple[dict[str, Any], Path, str]:
    raw = path.read_bytes()
    spec_digest = sha256(raw)
    if expected_digest and spec_digest != expected_digest:
        raise ValueError("acceptance spec digest mismatch")
    spec = json.loads(raw)
    required = ("fixture_id", "fixture_version", "fixture_path", "fixture_digest",
                "suite_version", "suite_path", "suite_digest")
    if not isinstance(spec, dict) or any(not isinstance(spec.get(k), str) or not spec[k] for k in required):
        raise ValueError("invalid acceptance spec")
    timeout = spec.get("timeout_seconds", 10)
    if type(timeout) is not int or not 1 <= timeout <= 60:
        raise ValueError("timeout_seconds must be an integer from 1 to 60")
    if not safe_rel(spec["suite_path"]) or Path(spec["suite_path"]).name != "test_acceptance.py":
        raise ValueError("suite_path must be a safe path ending in test_acceptance.py")
    if not safe_rel(spec["fixture_path"]):
        raise ValueError("unsafe fixture_path")
    fixture = path.parent / spec["fixture_path"]
    if fixture.is_symlink() or not fixture.is_file() or not fixture.resolve().is_relative_to(path.parent.resolve()):
        raise ValueError("unsafe or missing fixture file")
    if any(parent.is_symlink() for parent in fixture.parents if parent != path.parent and parent.is_relative_to(path.parent)):
        raise ValueError("fixture path contains symlink")
    if sha256(fixture.read_bytes()) != spec["fixture_digest"]:
        raise ValueError("fixture digest mismatch")
    suite = path.parent / spec["suite_path"]
    if (suite.is_symlink() or not suite.is_file() or
            not suite.resolve().is_relative_to(path.parent.resolve())):
        raise ValueError("unsafe or missing suite file")
    if any(parent.is_symlink() for parent in list(suite.parents) if parent != path.parent and parent.is_relative_to(path.parent)):
        raise ValueError("suite path contains symlink")
    if sha256(suite.read_bytes()) != spec["suite_digest"]:
        raise ValueError("acceptance suite digest mismatch")
    return spec, suite, spec_digest


def bubblewrap_available() -> bool:
    binary = shutil.which("bwrap")
    if binary is None:
        return False
    try:
        probe = subprocess.run([binary, "--unshare-all", "--die-with-parent", "--ro-bind", "/usr", "/usr",
                                "--symlink", "usr/bin", "/bin", "--symlink", "usr/lib", "/lib",
                                "--symlink", "usr/lib64", "/lib64",
                                "--proc", "/proc", "--dev", "/dev", "/usr/bin/true"],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5)
        return probe.returncode == 0
    except (OSError, subprocess.TimeoutExpired):
        return False


def limits() -> None:
    resource.setrlimit(resource.RLIMIT_CPU, (15, 15))
    resource.setrlimit(resource.RLIMIT_AS, (1024 * 1024 * 1024, 1024 * 1024 * 1024))
    resource.setrlimit(resource.RLIMIT_FSIZE, (64 * 1024 * 1024, 64 * 1024 * 1024))
    resource.setrlimit(resource.RLIMIT_NOFILE, (64, 64))
    # Linux compares RLIMIT_NPROC against the whole user's process count, not the
    # sandbox's; a tight soft limit makes unshare() fail with EAGAIN before the
    # sandbox starts. 512 still bounds fork bombs inside the sandbox.
    resource.setrlimit(resource.RLIMIT_NPROC, (512, 512))


def parse_unittest(output: str, exit_code: int) -> dict[str, int] | None:
    ran = re.search(r"(?m)^Ran (\d+) tests? in [\d.]+s\s*$", output)
    if not ran:
        return None
    total = int(ran.group(1))
    failed = errors = skipped = 0
    summary = re.search(r"(?m)^FAILED \(([^\n]+)\)\s*$", output)
    ok = re.search(r"(?m)^OK(?: \(([^\n]+)\))?\s*$", output)
    if exit_code == 0 and not ok or exit_code != 0 and not summary:
        return None
    details = summary.group(1) if summary else (ok.group(1) or "")
    for key, value in re.findall(r"(failures|errors|skipped)=(\d+)", details):
        if key == "failures":
            failed = int(value)
        elif key == "errors":
            errors = int(value)
        else:
            skipped = int(value)
    if total < 1 or failed + errors + skipped > total:
        return None
    return {"total": total, "passed": total - failed - errors - skipped,
            "failed": failed, "errors": errors, "skipped": skipped}


def run_process(work: Path, suite: Path, timeout: int, trusted_local: bool) -> tuple[str, int | None, str, float, list[str]]:
    command = [sys.executable, "-m", "unittest", "discover", "-s", "/suite", "-p", "test_acceptance.py", "-v"]
    env = {"PATH": "/usr/bin:/bin", "PYTHONPATH": "/work", "PYTHONDONTWRITEBYTECODE": "1",
           "HOME": "/tmp", "LANG": "C.UTF-8"}
    if trusted_local:
        command = [sys.executable, "-m", "unittest", "discover", "-s", str(suite.parent),
                   "-p", "test_acceptance.py", "-v"]
        env["PYTHONPATH"] = str(work)
    else:
        binary = shutil.which("bwrap")
        command = [binary, "--unshare-all", "--die-with-parent", "--new-session", "--clearenv",
                   "--ro-bind", "/usr", "/usr", "--symlink", "usr/bin", "/bin",
                   "--symlink", "usr/lib", "/lib", "--symlink", "usr/lib64", "/lib64",
                   "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp",
                   "--dir", "/work", "--bind", str(work), "/work",
                   "--ro-bind", str(suite.parent), "/suite", "--chdir", "/work",
                   *[item for key, value in env.items() for item in ("--setenv", key, value)],
                   sys.executable, "-m", "unittest", "discover", "-s", "/suite",
                   "-p", "test_acceptance.py", "-v"]
    started = time.monotonic()
    with tempfile.TemporaryFile() as stream:
        process = subprocess.Popen(command, cwd=work, env=env, stdout=stream,
                                   stderr=subprocess.STDOUT, start_new_session=True, preexec_fn=limits)
        try:
            process.wait(timeout=timeout)
            status = "completed"
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            status = "timeout"
        stream.seek(0)
        data = stream.read(MAX_LOG_BYTES + 1)
    duration = round(time.monotonic() - started, 3)
    output = data[:MAX_LOG_BYTES].decode("utf-8", errors="replace")
    if len(data) > MAX_LOG_BYTES:
        output += "\n[log truncated]\n"
    return status, process.returncode, output, duration, command


def run_acceptance(source: str | Path, spec_path: str | Path, output: str | Path,
                   *, issue_id: str | None = None, expected_spec_digest: str | None = None,
                   trusted_local: bool = False) -> list[dict[str, Any]]:
    root = Path(source).resolve(strict=True)
    target = Path(output).resolve()
    spec_path = Path(spec_path).resolve(strict=True)
    if target.exists() or target.is_relative_to(root) or target == root:
        raise ValueError("output must be new and outside historical source")
    if root.is_relative_to(target):
        raise ValueError("output cannot contain source")
    spec, suite, spec_digest = load_spec(spec_path, expected_spec_digest)
    snapshots = [(p, verify_snapshot(p, root)) for p in snapshot_paths(root)]
    candidates = [(p, s) for p, s in snapshots if s.get("round") in (1, 2) and
                  (issue_id is None or s.get("issue_id") == issue_id)]
    if not issue_id and len({s.get("issue_id") for _, s in candidates}) > 1:
        raise ValueError("multiple issues; specify issue_id")
    if not candidates:
        raise ValueError("no r1/r2 snapshot for issue")
    spec_bytes, suite_bytes = spec_path.read_bytes(), suite.read_bytes()
    fixture_bytes = (spec_path.parent / spec["fixture_path"]).read_bytes()
    if (sha256(spec_bytes) != spec_digest or sha256(suite_bytes) != spec["suite_digest"]
            or sha256(fixture_bytes) != spec["fixture_digest"]):
        raise ValueError("acceptance spec, suite or fixture changed during copy")
    target.mkdir(parents=True, mode=0o700)
    (target / "spec.json").write_bytes(spec_bytes)
    (target / "fixture.txt").write_bytes(fixture_bytes)
    suite_copy = target / "test_acceptance.py"
    suite_copy.write_bytes(suite_bytes)
    isolated = bubblewrap_available() if not trusted_local else False
    records = []
    for manifest_path, snap in candidates:
        round_no = snap["round"]
        item = {"schema_version": "1", "runner_version": RUNNER_VERSION,
                "issue_id": snap.get("issue_id"), "run_id": snap.get("run_id"), "round": round_no,
                "snapshot_id": snap.get("snapshot_id"), "manifest_digest": snap["manifest"].get("sha256"),
                "fixture_id": spec["fixture_id"], "fixture_version": spec["fixture_version"],
                "fixture_digest": spec["fixture_digest"],
                "fixture_path": "fixture.txt",
                "spec_digest": spec_digest, "suite_version": spec["suite_version"],
                "spec_path": "spec.json",
                "suite_digest": spec["suite_digest"], "suite_path": "test_acceptance.py",
                "command": None,
                "isolation": "trusted_local" if trusted_local else "bubblewrap",
                "exit_code": None, "duration_seconds": 0.0,
                "timeout_seconds": spec.get("timeout_seconds", 10),
                "counts": None, "status": "infrastructure_failure", "reason": None,
                "log_path": f"r{round_no}.log"}
        log = ""
        if snap["status"] != "valid":
            item.update(status="verification_failed", reason="invalid snapshot: " + "; ".join(snap["issues"]))
        elif not isolated and not trusted_local:
            item["reason"] = "bubblewrap isolation unavailable; trusted-local opt-in required"
        else:
            with tempfile.TemporaryDirectory(prefix="acceptance-work-") as temp:
                work = Path(temp)
                manifest = json.loads(manifest_path.read_text())
                for rel in manifest["changed_files"]:
                    if not safe_rel(rel):
                        raise ValueError("unsafe snapshot path")
                    dest = work / rel
                    dest.parent.mkdir(parents=True, exist_ok=True)
                    dest.write_bytes((manifest_path.parent / "files" / rel).read_bytes())
                try:
                    status, exit_code, log, duration, command = run_process(
                        work, suite_copy, item["timeout_seconds"], trusted_local)
                    item.update(status=status, exit_code=exit_code, duration_seconds=duration,
                                command=command)
                    if status == "completed":
                        counts = parse_unittest(log, exit_code)
                        if counts is None:
                            item.update(status="infrastructure_failure", reason="unittest summary unavailable or invalid")
                        else:
                            item["counts"] = counts
                    else:
                        item["reason"] = "acceptance test timed out"
                except (OSError, subprocess.SubprocessError) as exc:
                    item["reason"] = f"test process failed: {type(exc).__name__}: {exc}"
        (target / item["log_path"]).write_text(log)
        item["log_digest"] = sha256(log.encode())
        item["record_path"] = f"r{round_no}.json"
        (target / item["record_path"]).write_text(json.dumps(item, indent=2, sort_keys=True) + "\n")
        records.append(item)
    return records


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("--spec", type=Path, required=True)
    parser.add_argument("--spec-digest", required=True, help="expected SHA-256 of spec JSON")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--issue-id")
    parser.add_argument("--trusted-local", action="store_true", help="only for trusted fixtures")
    args = parser.parse_args()
    records = run_acceptance(args.source, args.spec, args.output, issue_id=args.issue_id,
                             expected_spec_digest=args.spec_digest, trusted_local=args.trusted_local)
    print(json.dumps([{"round": r["round"], "status": r["status"], "counts": r["counts"]} for r in records]))


if __name__ == "__main__":
    main()
