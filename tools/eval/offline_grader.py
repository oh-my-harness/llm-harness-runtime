"""离线核验 Coding Team 产物；不运行产品、测试或模型。"""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
from typing import Any

SCHEMA_VERSION = "1"
GRADER_VERSION = "p1.1"
STATUSES = {"measured", "blocked", "not_measured", "not_applicable", "measurement_error"}


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def reference(path: Path, root: Path, *, line: int | None = None) -> dict[str, Any]:
    data = path.read_bytes()
    ref: dict[str, Any] = {"path": str(path.relative_to(root)), "sha256": digest(data)}
    if line is not None:
        ref["line"] = line
    return ref


def metric(status: str, evidence: list[dict[str, Any]], value: Any = None,
           denominator: int | None = None, reason: str | None = None) -> dict[str, Any]:
    assert status in STATUSES
    result: dict[str, Any] = {"status": status, "evidence": evidence}
    if status == "measured":
        result["value"] = value
    if denominator is not None:
        result["denominator"] = denominator
    if reason:
        result["reason"] = reason
    return result


def safe_rel(value: str) -> bool:
    path = PurePosixPath(value)
    return bool(value) and not path.is_absolute() and all(
        part not in ("", ".", "..") for part in value.split("/")
    ) and "\\" not in value


def snapshot_paths(root: Path) -> list[Path]:
    # 支持 run/workspace/.pipeline 和直接传入历史 workspace。
    paths = list(root.glob(".pipeline/snapshots/*/r*/snapshot.json"))
    paths += list(root.glob("workspace/.pipeline/snapshots/*/r*/snapshot.json"))
    return sorted(paths)


def verify_snapshot(path: Path, root: Path) -> dict[str, Any]:
    try:
        manifest_ref = reference(path, root)
    except OSError as exc:
        return {"manifest": {"path": str(path.relative_to(root))}, "status": "unavailable",
                "issues": [f"cannot read manifest: {exc}"]}
    out: dict[str, Any] = {"manifest": manifest_ref, "status": "valid", "issues": []}
    try:
        manifest = json.loads(path.read_bytes())
        if not isinstance(manifest, dict):
            raise ValueError("manifest is not an object")
        if ("file_hashes" not in manifest and manifest.get("changed_files") == []
                and isinstance(manifest.get("round"), int)):
            out.update(status="unavailable", round=manifest["round"],
                       run_id=manifest.get("run_id"), issue_id=manifest.get("issue_id"),
                       issues=["legacy empty manifest lacks file_hashes; integrity unavailable"])
            return out
        paths = manifest["changed_files"]
        hashes = manifest["file_hashes"]
        content_hash = manifest["content_hash"]
        run_id = manifest["run_id"]
        round_no = manifest["round"]
        issue_id = manifest["issue_id"]
        if (not isinstance(paths, list) or not isinstance(hashes, dict)
                or not isinstance(content_hash, str) or not isinstance(round_no, int)
                or not isinstance(run_id, str) or not isinstance(issue_id, str)):
            raise ValueError("invalid manifest field types")
        if (path.parent.name != f"r{round_no}" or path.parent.parent.name != run_id
                or len(set(paths)) != len(paths) or set(paths) != set(hashes)):
            raise ValueError("manifest path, run, round or file list mismatch")
        out.update({"run_id": run_id, "round": round_no, "issue_id": issue_id,
                    "snapshot_id": manifest.get("snapshot_id"), "files": []})
        aggregate = hashlib.sha256()
        for rel in sorted(paths):
            if not isinstance(rel, str) or not safe_rel(rel):
                raise ValueError(f"unsafe file path: {rel!r}")
            file = path.parent / "files" / rel
            if file.is_symlink() or not file.is_file() or not file.resolve().is_relative_to((path.parent / "files").resolve()):
                out["issues"].append(f"missing or unsafe captured file: {rel}")
                continue
            data = file.read_bytes()
            ref = reference(file, root)
            out["files"].append(ref)
            if ref["sha256"] != hashes[rel]:
                out["issues"].append(f"file hash mismatch: {rel}")
            # pipeline.rs hash_input_for: decimal UTF-8 byte length, US, path,
            # US, decimal byte length, RS, then original bytes.
            aggregate.update(f"{len(rel.encode('utf-8'))}\x1f{rel}\x1f{len(data)}\x1e".encode())
            aggregate.update(data)
        if len(out["files"]) == len(paths) and aggregate.hexdigest() != content_hash:
            out["issues"].append("aggregate content_hash mismatch")
        expected_id = f"{issue_id}#r{round_no}#{content_hash}"
        if manifest.get("snapshot_id") != expected_id:
            out["issues"].append("snapshot_id mismatch")
        out["task_start_baseline"] = manifest.get("task_start_baseline")
        out["task_relative_changed_paths"] = manifest.get("task_relative_changed_paths")
        out["task_relative_diff"] = manifest.get("task_relative_diff")
        if manifest.get("task_relative_diff") is not None:
            patch_name = manifest["task_relative_diff"]
            if patch_name != "task-relative.patch":
                out["issues"].append("unexpected task-relative patch name")
            else:
                patch_file = path.parent / patch_name
                if patch_file.is_symlink() or not patch_file.is_file():
                    out["issues"].append("task-relative patch missing or unsafe")
                else:
                    out["patch"] = reference(patch_file, root)
    except (OSError, ValueError, KeyError, TypeError, json.JSONDecodeError) as exc:
        out["issues"].append(f"malformed manifest: {exc}")
    if out["issues"]:
        out["status"] = "malformed"
    return out


def verify_relationships(snapshots: list[dict[str, Any]]) -> list[str]:
    """核查 manifest 声明的 r0 关系；不把缺少 r2 视为损坏。"""
    issues = []
    by_run = {(s.get("run_id"), s.get("round")): s for s in snapshots if s["status"] == "valid"}
    for (run_id, round_no) in by_run:
        if isinstance(round_no, int) and round_no > 1 and (run_id, round_no - 1) not in by_run:
            issues.append(f"{run_id}/r{round_no}: preceding round unavailable")
    for snap in snapshots:
        if snap["status"] != "valid" or snap["round"] == 0:
            continue
        base = by_run.get((snap["run_id"], 0))
        if base is None:
            issues.append(f"{snap['run_id']}/r{snap['round']}: r0 unavailable")
            continue
        declared = snap.get("task_start_baseline")
        if declared is None:
            issues.append(f"{snap['run_id']}/r{snap['round']}: task_start_baseline unavailable")
            continue
        expected_suffix = f".pipeline/snapshots/{snap['run_id']}/r0/snapshot.json"
        if not str(declared).replace("\\", "/").endswith(expected_suffix):
            issues.append(f"{snap['run_id']}/r{snap['round']}: baseline reference mismatch")
        base_hashes = {item["path"].split("/files/", 1)[-1]: item["sha256"] for item in base["files"]}
        current_hashes = {item["path"].split("/files/", 1)[-1]: item["sha256"] for item in snap["files"]}
        changed = sorted(path for path in base_hashes.keys() | current_hashes.keys()
                         if base_hashes.get(path) != current_hashes.get(path))
        if snap.get("task_relative_changed_paths") != changed:
            issues.append(f"{snap['run_id']}/r{snap['round']}: task-relative changed paths mismatch")
        patch = snap.get("task_relative_diff")
        if changed and patch != "task-relative.patch":
            issues.append(f"{snap['run_id']}/r{snap['round']}: task-relative patch reference unavailable")
        if not changed and patch is not None:
            issues.append(f"{snap['run_id']}/r{snap['round']}: unexpected task-relative patch")
    return issues


def read_events(path: Path, root: Path) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    record: dict[str, Any] = {"status": "missing", "reference": None, "issues": []}
    if not path.exists():
        return [], record
    try:
        data = path.read_bytes()
    except OSError as exc:
        record.update(status="unavailable", issues=[str(exc)])
        return [], record
    record.update(status="available", reference={"path": str(path.relative_to(root)), "sha256": digest(data)})
    rows = []
    for number, raw in enumerate(data.splitlines(), 1):
        try:
            event = json.loads(raw)
            if not isinstance(event, dict) or not isinstance(event.get("type"), str):
                raise ValueError("event lacks type")
            rows.append({"event": event, "reference": {**record["reference"], "line": number}})
        except (ValueError, json.JSONDecodeError) as exc:
            record["issues"].append(f"line {number}: {exc}")
    if record["issues"]:
        record["status"] = "malformed"
    return rows, record


def artifact(root: Path, relative: str) -> dict[str, Any]:
    path = root / relative
    if not path.exists():
        return {"status": "missing"}
    try:
        return {"status": "available", "reference": reference(path, root)}
    except OSError as exc:
        return {"status": "unavailable", "reason": str(exc)}


def structured_acceptance(directory: Path, snapshots: list[dict[str, Any]]) -> tuple[list[dict[str, Any]], list[str]]:
    """Validate runner records and their complete attribution chain."""
    results, issues = [], []
    if not directory.is_dir():
        return results, issues
    for path in sorted(directory.glob("r[12].json")):
        try:
            if path.is_symlink():
                raise ValueError("record is a symlink")
            obj = json.loads(path.read_text())
            required = ("snapshot_id", "manifest_digest", "fixture_digest", "fixture_path", "suite_digest", "suite_version",
                        "spec_digest", "spec_path", "suite_path", "log_path", "log_digest", "command",
                        "status", "round", "isolation")
            if not isinstance(obj, dict) or any(k not in obj for k in required):
                raise ValueError("structured record fields missing")
            snap = next((s for s in snapshots if s.get("snapshot_id") == obj["snapshot_id"]
                         and s.get("round") == obj["round"] and s["status"] == "valid"), None)
            if snap is None or obj["round"] not in (1, 2):
                raise ValueError("record does not identify a valid coder snapshot")
            if snap["manifest"]["sha256"] != obj["manifest_digest"]:
                raise ValueError("manifest digest mismatch")
            record_ref = reference(path, directory)
            record_ref["path"] = str(path)
            refs = [record_ref, snap["manifest"], *snap["files"]]
            for key, expected in (("spec_path", obj["spec_digest"]),
                                  ("fixture_path", obj["fixture_digest"]),
                                  ("suite_path", obj["suite_digest"]),
                                  ("log_path", obj["log_digest"])):
                rel = obj[key]
                if not isinstance(rel, str) or not safe_rel(rel):
                    raise ValueError(f"unsafe {key}")
                file = directory / rel
                if file.is_symlink() or not file.is_file() or not file.resolve().is_relative_to(directory.resolve()):
                    raise ValueError(f"{key} unavailable")
                ref = reference(file, directory)
                ref["path"] = str(file)
                if ref["sha256"] != expected:
                    raise ValueError(f"{key} digest mismatch")
                refs.append(ref)
            spec = json.loads((directory / obj["spec_path"]).read_text())
            if (spec.get("suite_digest") != obj["suite_digest"] or
                    spec.get("suite_version") != obj["suite_version"] or
                    spec.get("fixture_id") != obj.get("fixture_id") or
                    spec.get("fixture_version") != obj.get("fixture_version") or
                    not isinstance(spec.get("fixture_path"), str) or
                    spec.get("fixture_digest") != obj.get("fixture_digest")):
                raise ValueError("record and acceptance spec disagree")
            status = obj["status"]
            if status == "completed":
                c = obj.get("counts")
                if (not isinstance(obj.get("command"), list) or not obj["command"] or
                    any(not isinstance(part, str) or not part for part in obj["command"]) or
                    not isinstance(c, dict) or any(not isinstance(c.get(k), int) or c[k] < 0
                    for k in ("total", "passed", "failed", "errors", "skipped"))
                    or c["total"] != sum(c[k] for k in ("passed", "failed", "errors", "skipped"))
                    or not isinstance(obj.get("exit_code"), int)):
                    raise ValueError("invalid unittest counts or exit code")
                if (obj["exit_code"] == 0) != (c["failed"] == 0 and c["errors"] == 0):
                    raise ValueError("exit code and test counts disagree")
            results.append({"record": obj, "evidence": refs})
        except (OSError, ValueError, TypeError, KeyError, json.JSONDecodeError) as exc:
            issues.append(f"{path.name}: {exc}")
    return results, issues


def grade(source: str | Path, *, acceptance_dir: str | Path | None = None) -> dict[str, Any]:
    """只读评分入口；返回不含运行时间的确定性结果。"""
    root = Path(source).resolve(strict=True)
    if not root.is_dir():
        raise ValueError("source must be a directory")
    snapshots = [verify_snapshot(path, root) for path in snapshot_paths(root)]
    relationship_issues = verify_relationships(snapshots)
    evidence_dir = root / "evidence"
    events, event_record = read_events(evidence_dir / "events.jsonl", root)
    terminal = artifact(root, "evidence/human-review-package.md")
    test_text = artifact(root, "evidence/independent-test.txt")
    test_record = artifact(root, "evidence/independent-test.json")
    acceptance_path = Path(acceptance_dir).resolve() if acceptance_dir is not None else root / "evidence/independent-tests"
    acceptance, acceptance_issues = structured_acceptance(acceptance_path, snapshots)
    refs = [item["reference"] for item in events]
    types = [item["event"]["type"] for item in events]
    terminal_types = {"pipeline_completed", "pipeline_escalated", "pipeline_failed"}
    terminal_rows = [item for item in events if item["event"]["type"] in terminal_types]
    terminal_ref = [item["reference"] for item in terminal_rows[-1:]]
    cause_refs: list[dict[str, Any]] = []
    provider_error = False
    for item in events:
        event = item["event"]
        if event["type"] == "agent_error" and "provider" in str(event.get("error", "")).lower():
            provider_error = True
            cause_refs.append(item["reference"])
    # 旧运行只有人工报告保留 provider 故障；显式降低根因置信度。
    for relative in ("evidence/provider-failure.txt", "evidence/report.md"):
        path = root / relative
        if path.is_file() and "llm provider error" in path.read_text(errors="replace").lower():
            provider_error = True
            cause_refs.append(reference(path, root))
    reason = str(terminal_rows[-1]["event"].get("reason", "")).lower() if terminal_rows else ""
    if "pipeline_completed" in types:
        outcome = "completed"
    elif provider_error and terminal_rows:
        outcome = "provider_blocked"
    elif "cancel" in reason:
        outcome = "cancelled"
    elif "timed out" in reason or "timeout" in reason:
        outcome = "timed_out"
    elif "pipeline_failed" in types:
        outcome = "runtime_failed"
    elif "pipeline_escalated" in types:
        outcome = "unknown"
    elif "pipeline_started" in types:
        outcome = "incomplete"
    else:
        outcome = "unknown"
    outcome_refs = terminal_ref + cause_refs
    if not outcome_refs and refs:
        outcome_refs = refs[:1]
    uncertainty = ("provider error is reported; exact upstream cause is unverified"
                   if outcome == "provider_blocked" else None)
    valid = [s for s in snapshots if s["status"] == "valid"]
    bad = [s for s in snapshots if s["status"] == "malformed"]
    unavailable = [s for s in snapshots if s["status"] == "unavailable"]
    rounds = {s["round"] for s in valid}
    snapshot_refs = [s["manifest"] for s in snapshots]
    integrity = (metric("measurement_error", snapshot_refs, reason="damaged snapshot") if bad else
                 metric("not_measured", snapshot_refs, reason="legacy snapshot integrity unavailable") if unavailable else
                 metric("measured", snapshot_refs, True) if snapshots else
                 metric("not_measured", [], reason="no snapshot manifests"))
    relationships = (metric("measurement_error", snapshot_refs, reason="; ".join(relationship_issues))
                     if any("mismatch" in issue or "unexpected" in issue for issue in relationship_issues)
                     else metric("not_measured", snapshot_refs, reason="; ".join(relationship_issues))
                     if relationship_issues else metric("measured", snapshot_refs, True)
                     if 0 in rounds and len(rounds) > 1 else metric("not_measured", snapshot_refs,
                                                                      reason="no r0-to-revision pair"))
    baseline = (metric("measured", [s["manifest"] for s in valid if s["round"] == 0], True)
                if 0 in rounds else metric("not_measured", snapshot_refs, reason="r0 absent"))
    revision = (metric("not_measured", snapshot_refs, reason="r2 absent; no revision inference")
                if 1 in rounds and 2 not in rounds else
                metric("blocked", outcome_refs, reason="no coder snapshot") if outcome == "provider_blocked" else
                metric("not_measured", snapshot_refs, reason="same-suite r1/r2 tests unavailable"))
    code = metric("blocked", outcome_refs, reason="no coder output after provider failure") if outcome == "provider_blocked" else metric(
        "not_measured", snapshot_refs, reason="no attributable independent acceptance result")
    # 独立测试需显式绑定被测 Snapshot、套件、命令和退出状态。
    if test_record["status"] == "available":
        try:
            obj = json.loads((root / "evidence/independent-test.json").read_text())
            required = ("snapshot_id", "suite_digest", "suite_path", "command", "exit_code")
            if (not isinstance(obj, dict) or any(k not in obj for k in required)
                    or not isinstance(obj["exit_code"], int)
                    or not obj["suite_digest"] or not obj["command"]):
                raise ValueError("test attribution fields missing")
            match = next((s for s in valid if s["snapshot_id"] == obj["snapshot_id"]), None)
            if match is None or match["round"] == 0:
                raise ValueError("test does not identify a valid coder snapshot")
            if not safe_rel(obj["suite_path"]):
                raise ValueError("unsafe suite_path")
            suite_file = root / obj["suite_path"]
            if suite_file.is_symlink() or not suite_file.is_file() or not suite_file.resolve().is_relative_to(root):
                raise ValueError("test suite unavailable")
            suite_ref = reference(suite_file, root)
            if suite_ref["sha256"] != obj["suite_digest"]:
                raise ValueError("test suite digest mismatch")
            code = metric("measured", [test_record["reference"], match["manifest"], suite_ref],
                          obj["exit_code"] == 0,
                          reason="attributed acceptance-suite exit status; other requirements unverified")
        except (OSError, ValueError, TypeError, json.JSONDecodeError) as exc:
            code = metric("measurement_error", [test_record["reference"]], reason=str(exc))
    test_counts = metric("not_measured", [], reason="no structured independent acceptance result")
    if acceptance_issues:
        code = metric("measurement_error", [], reason="; ".join(acceptance_issues))
        test_counts = metric("measurement_error", [], reason="; ".join(acceptance_issues))
    elif acceptance:
        selected = sorted(acceptance, key=lambda item: item["record"]["round"])[-1]
        obj, evidence = selected["record"], selected["evidence"]
        latest_coder_round = max((s["round"] for s in valid if s["round"] in (1, 2)
                                  and s.get("issue_id") == obj.get("issue_id")), default=obj["round"])
        if obj["round"] < latest_coder_round:
            code = metric("not_measured", evidence, reason="latest coder snapshot lacks acceptance result")
        elif obj["status"] == "completed":
            counts = obj["counts"]
            test_counts = metric("measured", evidence, counts, denominator=counts["total"])
            if counts["errors"]:
                code = metric("measurement_error", evidence, reason="test execution errors")
            elif counts["skipped"]:
                code = metric("not_measured", evidence, reason="acceptance tests skipped")
            else:
                code = metric("measured", evidence, counts["failed"] == 0,
                              denominator=counts["total"], reason="independent unittest assertions")
        else:
            code = metric("not_measured", evidence, reason=obj.get("reason") or obj["status"])
        by_round = {item["record"]["round"]: item for item in acceptance}
        if 1 in by_round and 2 in by_round:
            first, second = by_round[1], by_round[2]
            a, b = first["record"], second["record"]
            if (a["status"] == b["status"] == "completed" and a["suite_digest"] == b["suite_digest"]
                    and a["suite_version"] == b["suite_version"]
                    and a.get("fixture_id") == b.get("fixture_id")
                    and a.get("fixture_version") == b.get("fixture_version")
                    and a.get("fixture_digest") == b.get("fixture_digest")
                    and a["counts"]["total"] == b["counts"]["total"]
                    and not a["counts"]["errors"] and not b["counts"]["errors"]):
                revision = metric("measured", first["evidence"] + second["evidence"],
                                  b["counts"]["passed"] - a["counts"]["passed"],
                                  denominator=a["counts"]["total"], reason="change in passed tests")
    execution = metric("measured", terminal_ref, outcome == "completed") if terminal_rows else metric(
        "not_measured", refs, reason="terminal event unavailable")
    event_parse = (metric("measurement_error", [event_record["reference"]], reason="malformed JSONL")
                   if event_record["status"] == "malformed" else
                   metric("measured", refs, True) if event_record["status"] == "available" else
                   metric("not_measured", [], reason="event JSONL missing"))
    event_integrity = metric("not_measured", refs,
                             reason="event stream completeness cannot be proven from JSONL alone")
    return {
        "schema_version": SCHEMA_VERSION, "grader_version": GRADER_VERSION,
        "source": str(root),
        "outcome": {"category": outcome, "evidence": outcome_refs, "uncertainty": uncertainty},
        "artifacts": {"snapshots": snapshots, "events": event_record,
                      "terminal_package": terminal, "independent_test_text": test_text,
                      "independent_test_record": test_record,
                      "structured_acceptance": acceptance, "structured_acceptance_issues": acceptance_issues},
        "metrics": {
            "snapshot_integrity": integrity, "task_start_baseline": baseline,
            "snapshot_relationship": relationships,
            "execution_completion": execution, "code_correctness": code,
            "acceptance_test_counts": test_counts,
            "revision_improvement": revision, "event_stream_integrity": event_integrity,
            "event_jsonl_parse": event_parse,
            "tokens": metric("not_measured", [], reason="workflow usage unavailable"),
            "api_cost": metric("not_measured", [], reason="workflow cost unavailable"),
            "human_attention": metric("not_measured", [], reason="interventions not fully captured"),
        },
    }


def render_report(result: dict[str, Any]) -> str:
    lines = ["# Offline evaluation", "", f"Source: `{result['source']}`",
             f"Outcome: **{result['outcome']['category']}**", "", "| Metric | Status | Value | Reason |",
             "|---|---|---|---|"]
    for name, item in result["metrics"].items():
        value = str(item["value"]) if "value" in item else "—"
        lines.append(f"| {name} | {item['status']} | {value} | {item.get('reason', '')} |")
    lines += ["", "Evidence references and SHA-256 values are in `result.json`.", ""]
    return "\n".join(lines)


def write_report(source: str | Path, output: str | Path,
                 *, acceptance_dir: str | Path | None = None) -> dict[str, Any]:
    root = Path(source).resolve(strict=True)
    target = Path(output).resolve()
    if target == root or target.is_relative_to(root):
        raise ValueError("output must be outside the source directory")
    result = grade(root, acceptance_dir=acceptance_dir)
    target.mkdir(parents=True, exist_ok=True)
    (target / "result.json").write_text(json.dumps(result, ensure_ascii=False, indent=2, sort_keys=True) + "\n")
    (target / "report.md").write_text(render_report(result))
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", help="preserved run or workspace directory")
    parser.add_argument("--output", required=True, help="separate report directory")
    parser.add_argument("--acceptance-dir", help="runner output directory")
    args = parser.parse_args()
    result = write_report(args.source, args.output, acceptance_dir=args.acceptance_dir)
    print(f"{result['outcome']['category']}: {Path(args.output) / 'result.json'}")


if __name__ == "__main__":
    main()
