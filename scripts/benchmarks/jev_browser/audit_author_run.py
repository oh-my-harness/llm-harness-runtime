"""Read-only reconciliation of paired author-suite counters and reported token usage."""

import argparse
import hashlib
import json
from pathlib import Path

from author_control import compare


COMPONENTS = {"qwen_http": "qwen_http", "laya_rpc": "laya_rpc", "laya_predict": "laya_predict",
              "laya_forward": "laya_forward", "tools": "tool"}


def validate_text_request(manifest, event):
    if event.get("network_route") != manifest["network"]["qwen_proxy"]:
        raise ValueError("Text-model proxy drift")
    body = event.get("request_body", {})
    if any(body.get(k) != v for k, v in manifest["common_qwen_parameters"].items()):
        raise ValueError("Text-model parameter drift")
    expected_model = manifest.get("text_model", manifest.get("qwen"))
    if expected_model and body.get("model") != expected_model:
        raise ValueError("Text-model identity drift")


def usage_summary(events):
    requests = [e for e in events if e["component"] == "qwen_http"]
    result = {"requests": len(requests)}
    for key in ("prompt_tokens", "completion_tokens"):
        known = [e["usage"][key] for e in requests
                 if type((e.get("usage") or {}).get(key)) is int and e["usage"][key] >= 0]
        result[key] = {"known_sum": sum(known), "unknown_requests": len(requests) - len(known),
                       "total": sum(known) if len(known) == len(requests) else None}
    return result


def paired_totals(pairs):
    a, b = (sum(p[f"{arm}_seconds"] for p in pairs) for arm in ("A", "B"))
    return {"pairs": len(pairs), "A_seconds": a, "B_seconds": b, "speed_ratio_A_over_B": a / b if b else None,
            "A_qwen": sum(p["A_qwen"] for p in pairs), "B_qwen": sum(p["B_qwen"] for p in pairs)}


def complete_round_analysis(rows, events, tasks):
    expected = {(task, arm) for task in tasks for arm in ("A", "B")}
    repeats = sorted({r.get("repeat", 1) for r in rows})
    complete = [rep for rep in repeats if {(r["task"], r["arm"]) for r in rows if r.get("repeat", 1) == rep} == expected]
    selected = [r for r in rows if r.get("repeat", 1) in complete]
    failed_http_pairs = {(e["task"], e.get("repeat", 1)) for e in events
                         if e["component"] == "qwen_http" and (e.get("http_status") != 200 or e.get("status") != "ok")}
    def describe(group):
        c = compare(group)
        clean = [p for p in c["both_success"] if (p["task"], p.get("repeat", 1)) not in failed_http_pairs]
        return {"groups": c["groups"], "quality_pairs": c["quality_pairs"],
                "both_success": paired_totals(c["both_success"]), "no_http_failure_both_success": paired_totals(clean)}
    return {"complete_repeats": complete, "complete_rounds": describe(selected),
            "per_repeat": {str(rep): describe([r for r in rows if r.get("repeat", 1) == rep]) for rep in repeats}}


def audit(directory):
    report = json.loads((directory / "report.json").read_text(encoding="utf-8"))
    manifest = json.loads((directory / "manifest.json").read_text(encoding="utf-8"))
    events = [json.loads(line) for line in (directory / "events.jsonl").read_text(encoding="utf-8").splitlines()]
    rows = report["rows"]
    if len(rows) != report["completed"] or report["completed"] + report["unstarted"] != len(manifest["order"]):
        raise ValueError("Attempt count mismatch")
    keys = ("task", "arm", "repeat") if "repeat" in manifest["order"][0] else ("task", "arm")
    if [{k: r[k] for k in keys} for r in rows] != manifest["order"][:len(rows)]:
        raise ValueError("Attempt order mismatch")
    if len({(r["task"], r["arm"], r.get("repeat", 1)) for r in rows}) != len(rows):
        raise ValueError("Duplicate attempt")
    for row in rows:
        attempt = [e for e in events if e["task"] == row["task"] and e.get("arm") == row["arm"]
                   and e.get("repeat", 1) == row.get("repeat", 1)]
        for key, component in COMPONENTS.items():
            if row["counts"][key] != sum(e["component"] == component for e in attempt):
                raise ValueError(f"Counter mismatch: {row['task']} {row['arm']} {key}")
        if row["arm"] == "A" and row["counts"]["laya_forward"]:
            raise ValueError("Pure text-model arm used Laya")
    for key, component in COMPONENTS.items():
        if report["counts_including_warmup"][key] != sum(e["component"] == component for e in events):
            raise ValueError(f"Session counter mismatch: {key}")
    for arm in ("A", "B"):
        if any(e.get("device") != "cuda:0" for e in events if e.get("arm") == arm and e["component"] == "laya_forward"):
            raise ValueError("Unexpected forward device")
    if manifest.get("controlled"):
        for event in events:
            if event["component"] != "qwen_http":
                continue
            validate_text_request(manifest, event)
    recalculated = compare(rows)
    if recalculated != report["comparison"]:
        raise ValueError("Comparison mismatch")
    matched_initial = []
    for row in rows:
        if row["arm"] != "A":
            continue
        other = next((r for r in rows if r["task"] == row["task"] and r["arm"] == "B"
                      and r.get("repeat", 1) == row.get("repeat", 1)), None)
        if other and row["initial_observation_sha256"] is not None and row["initial_observation_sha256"] == other["initial_observation_sha256"]:
            matched_initial.append(row["task"] if "repeat" not in row else {"task": row["task"], "repeat": row["repeat"]})
    sensitivity = compare([r for r in rows if r["task"] != manifest["order"][0]["task"]])
    return {"counters_and_comparison_verified": True, "completed": len(rows),
            "text_model": manifest.get("text_model", manifest.get("qwen")),
            "round_analysis": complete_round_analysis(rows, events, manifest["tasks"]),
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
