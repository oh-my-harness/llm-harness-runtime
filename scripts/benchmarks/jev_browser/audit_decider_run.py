"""Read-only reconciliation of all three arms; no inference and no modification of evidence."""

import argparse
import json
from pathlib import Path

import author_control
from audit_author_run import COMPONENTS, usage_summary, validate_text_request
import decider_adapter


def audit(directory):
    read = lambda name: json.loads((directory / name).read_text(encoding="utf-8"))
    report, manifest = read("report.json"), read("manifest.json")
    lines = lambda name: [json.loads(line) for line in (directory / name).read_text(encoding="utf-8").splitlines()]
    rows, events = lines("attempts.jsonl"), lines("events.jsonl")
    if rows != report["rows"] or len(rows) != report["completed"]:
        raise ValueError("Attempt ledger mismatch")
    if len(rows) + report["unstarted"] != len(manifest["order"]):
        raise ValueError("Planned attempt count mismatch")
    keys = ("task", "arm", "repeat")
    if [{k: row[k] for k in keys} for row in rows] != manifest["order"][:len(rows)]:
        raise ValueError("Attempt order mismatch")
    if len({tuple(row[k] for k in keys) for row in rows}) != len(rows):
        raise ValueError("Duplicate attempt")
    components = {**COMPONENTS, "decider_rpc": "decider_rpc", "decider_predict": "decider_predict",
                  "decider_forward": "decider_forward"}
    for row in rows:
        attempt = [e for e in events if all(e.get(k) == row[k] for k in keys)]
        for counter, component in components.items():
            if row["counts"][counter] != sum(e["component"] == component for e in attempt):
                raise ValueError(f"Counter mismatch: {row['task']} {row['arm']} {counter}")
        if row["arm"] != "B" and row["counts"]["laya_forward"]:
            raise ValueError("Laya inference outside B")
        if row["arm"] != "C" and row["counts"]["decider_forward"]:
            raise ValueError("Decider inference outside C")
    for counter, component in components.items():
        if report["counts_including_warmup"][counter] != sum(e["component"] == component for e in events):
            raise ValueError(f"Global counter mismatch: {counter}")
    for event in events:
        if event["component"] == "qwen_http":
            validate_text_request(manifest, event)
        if event["component"] in ("laya_forward", "decider_forward") and event.get("device") != "cuda:0":
            raise ValueError("Unexpected inference device")
        if event["component"] == "decision_http" and event.get("http_status") == 200:
            expected = "laya-browser" if event["arm"] == "B" else manifest["decider"]["version"]
            if event["arm"] not in ("B", "C") or event.get("model") != expected:
                raise ValueError("Decision model identity drift")
    comparison = decider_adapter.compare(rows)
    if comparison != report["comparison"]:
        raise ValueError("Three-arm comparison mismatch")
    totals = {}
    for arm in ("A", "B", "C"):
        selected = [e for e in events if e.get("arm") == arm and e["task"] in manifest["tasks"]]
        summed = lambda component: sum(e.get("elapsed_ms", 0) for e in selected if e["component"] == component) / 1000
        requests = [e for e in selected if e["component"] == "qwen_http"]
        totals[arm] = {"model_call_seconds": summed("qwen_http") + summed("decision_http"),
                       "flash_http_seconds": summed("qwen_http"), "decision_http_seconds": summed("decision_http"),
                       "tool_seconds": summed("tool"), "browser_start_seconds": summed("browser_start"),
                       "backoff_seconds": summed("provider_backoff"),
                       "http_200": sum(e.get("http_status") == 200 for e in requests),
                       "http_non_200": sum("http_status" in e and e["http_status"] != 200 for e in requests),
                       "http_transport_error": sum("http_status" not in e for e in requests),
                       "tokens": usage_summary(selected),
                       "singleton_rows": sum(e.get("singleton_rows", 0) for e in selected if e["component"] == "decider_input")}
    required = {(task, arm) for task in manifest["tasks"] for arm in ("A", "B", "C")}
    complete = [rep for rep in range(1, manifest["repeats"] + 1)
                if {(r["task"], r["arm"]) for r in rows if r["repeat"] == rep} == required]
    matching = {}
    for left, right in (("A", "B"), ("A", "C"), ("B", "C")):
        right_rows = {(r["task"], r["repeat"]): r for r in rows if r["arm"] == right}
        matching[f"{left}_vs_{right}"] = sum(
            bool(r["initial_observation_sha256"]) and r["initial_observation_sha256"] ==
            right_rows.get((r["task"], r["repeat"]), {}).get("initial_observation_sha256")
            for r in rows if r["arm"] == left)
    return {"verified": True, "completed": len(rows), "unstarted": report["unstarted"],
            "complete_repeats": complete, "comparison": comparison, "timing_and_usage": totals,
            "matching_initial_observations": matching,
            "per_repeat": {str(rep): decider_adapter.compare([r for r in rows if r["repeat"] == rep])
                           for rep in range(1, manifest["repeats"] + 1)},
            "raw_sha256": {name: decider_adapter.file_sha(directory / name)
                           for name in ("report.json", "manifest.json", "attempts.jsonl", "events.jsonl")}}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    print(json.dumps(audit(args.directory), ensure_ascii=False, indent=2))
