"""Read-only reconciliation of paired author-suite counters and reported token usage."""

import argparse
import hashlib
import json
from pathlib import Path

from author_control import compare


COMPONENTS = {"qwen_http": "qwen_http", "laya_rpc": "laya_rpc", "laya_predict": "laya_predict",
              "laya_forward": "laya_forward", "tools": "tool"}


def usage_summary(events):
    requests = [e for e in events if e["component"] == "qwen_http"]
    result = {"requests": len(requests)}
    for key in ("prompt_tokens", "completion_tokens"):
        known = [e["usage"][key] for e in requests
                 if type((e.get("usage") or {}).get(key)) is int and e["usage"][key] >= 0]
        result[key] = {"known_sum": sum(known), "unknown_requests": len(requests) - len(known),
                       "total": sum(known) if len(known) == len(requests) else None}
    return result


def audit(directory):
    report = json.loads((directory / "report.json").read_text(encoding="utf-8"))
    manifest = json.loads((directory / "manifest.json").read_text(encoding="utf-8"))
    events = [json.loads(line) for line in (directory / "events.jsonl").read_text(encoding="utf-8").splitlines()]
    rows = report["rows"]
    if len(rows) != report["completed"] or report["completed"] + report["unstarted"] != len(manifest["order"]):
        raise ValueError("Attempt count mismatch")
    if [{"task": r["task"], "arm": r["arm"]} for r in rows] != manifest["order"][:len(rows)]:
        raise ValueError("Attempt order mismatch")
    if len({(r["task"], r["arm"]) for r in rows}) != len(rows):
        raise ValueError("Duplicate attempt")
    for row in rows:
        attempt = [e for e in events if e["task"] == row["task"] and e.get("arm") == row["arm"]]
        for key, component in COMPONENTS.items():
            if row["counts"][key] != sum(e["component"] == component for e in attempt):
                raise ValueError(f"Counter mismatch: {row['task']} {row['arm']} {key}")
        if row["arm"] == "A" and row["counts"]["laya_forward"]:
            raise ValueError("Pure Qwen arm used Laya")
    for key, component in COMPONENTS.items():
        if report["counts_including_warmup"][key] != sum(e["component"] == component for e in events):
            raise ValueError(f"Session counter mismatch: {key}")
    for arm in ("A", "B"):
        if any(e.get("device") != "cuda:0" for e in events if e.get("arm") == arm and e["component"] == "laya_forward"):
            raise ValueError("Unexpected forward device")
    recalculated = compare(rows)
    if recalculated != report["comparison"]:
        raise ValueError("Comparison mismatch")
    matched_initial = []
    for row in rows:
        if row["arm"] != "A":
            continue
        other = next((r for r in rows if r["task"] == row["task"] and r["arm"] == "B"), None)
        if other and row["initial_observation_sha256"] is not None and row["initial_observation_sha256"] == other["initial_observation_sha256"]:
            matched_initial.append(row["task"])
    sensitivity = compare([r for r in rows if r["task"] != manifest["order"][0]["task"]])
    return {"counters_and_comparison_verified": True, "completed": len(rows),
            "token_usage_from_events": {arm: usage_summary([e for e in events if e.get("arm") == arm]) for arm in ("A", "B")},
            "matching_initial_tasks": matched_initial,
            "exclude_first_pair_speed_ratio": sensitivity["paired_speed_ratio_A_over_B"],
            "exclude_first_pair_both_pass": sensitivity["quality_pairs"]["both_pass"],
            "raw_sha256": {name: hashlib.sha256((directory / name).read_bytes()).hexdigest()
                           for name in ("report.json", "events.jsonl", "attempts.jsonl", "manifest.json")}}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    print(json.dumps(audit(args.directory), ensure_ascii=False, indent=2))
