"""Shared audit events. Offline tests are not model experiment results."""

from contextlib import contextmanager
import json
from pathlib import Path
import statistics
import time


def dump(path, value):
    with Path(path).open("x", encoding="utf-8") as stream:
        json.dump(value, stream, ensure_ascii=True, indent=2)


class Trace:
    def __init__(self, path, secret):
        self.stream = Path(path).open("x", encoding="utf-8")
        self.secret = secret
        self.events = []

    def write(self, value):
        serialized = json.dumps(value, ensure_ascii=True)
        if self.secret:
            serialized = serialized.replace(self.secret, "[REDACTED]")
        self.stream.write(serialized + "\n")
        self.stream.flush()

    @contextmanager
    def event(self, component, step, purpose, **fields):
        event = {"id": len(self.events) + 1, "component": component,
                 "step": step, "purpose": purpose, **fields}
        self.write({"phase": "start", **event})
        start = time.perf_counter_ns()
        try:
            yield event
            event["status"] = "ok"
        except Exception as error:
            event.update(status="error", error_type=type(error).__name__)
            if isinstance(getattr(error, "code", None), int):
                event["http_status"] = error.code
            raise
        finally:
            event["elapsed_ms"] = (time.perf_counter_ns() - start) / 1e6
            self.events.append(event)
            self.write({"phase": "end", **event})

    def close(self):
        self.stream.close()


def distribution(values):
    values = sorted(values)
    if not values:
        return {"n": 0, "mean_ms": None, "median_ms": None, "p95_ms": None}
    index = (len(values) - 1) * 0.95
    lower = int(index)
    upper = min(lower + 1, len(values) - 1)
    return {"n": len(values), "mean_ms": statistics.mean(values),
            "median_ms": statistics.median(values),
            "p95_ms": values[lower] + (values[upper] - values[lower]) * (index - lower)}


def summarize(results):
    report = {"scope": "development_only_not_quality_equivalence_evidence", "arms": {}, "pairs": []}
    indexed = {}
    for row in results:
        key = row["task_id"], row["repetition"], row["arm"]
        if key in indexed:
            raise ValueError("Duplicate task/repetition/arm result")
        indexed[key] = row
    for arm in ("qwen", "qwen_laya"):
        rows = [r for r in results if r["arm"] == arm]
        passed = sum(r["success"] for r in rows)
        times = [r["elapsed_ms"] for r in rows if r["elapsed_ms"] is not None]
        report["arms"][arm] = {
            "attempts": len(rows), "independent_tasks": len({r["task_id"] for r in rows}),
            "passed": passed, "pass_rate": passed / len(rows) if rows else None,
            "all_latency": distribution(times),
            "successful_latency": distribution([r["elapsed_ms"] for r in rows if r["success"]]),
            "failed_latency": distribution([r["elapsed_ms"] for r in rows
                                             if not r["success"] and r["elapsed_ms"] is not None]),
            "setup_failures": sum(r["elapsed_ms"] is None for r in rows),
            "qwen_calls": sum(r["calls"]["qwen"] for r in rows),
            "laya_calls": sum(r["calls"]["laya"] for r in rows),
            "laya_forward_attempts": sum(r["laya_forward_attempts"] for r in rows),
            "tool_calls": sum(r["tool_calls"] for r in rows),
            "qwen_by_purpose": {purpose: sum(r["qwen_by_purpose"].get(purpose, 0) for r in rows)
                                for purpose in ("decision", "generation", "mixed")},
            "retry_calls": sum(r["retry_calls"] for r in rows),
            "stale_decisions": sum(r["stale_decisions"] for r in rows),
            "amortized_task_ms_per_success": sum(times) / passed if passed else None,
            "amortized_whole_attempt_ms_per_success": sum(r["whole_attempt_ms"] for r in rows) / passed if passed else None,
        }
    a_rate, b_rate = (report["arms"][arm]["pass_rate"] for arm in ("qwen", "qwen_laya"))
    report["pass_rate_delta_pp"] = 100 * (b_rate - a_rate) if a_rate is not None and b_rate is not None else None
    report["pair_outcomes"] = {key: 0 for key in ("both_pass", "qwen_only", "hybrid_only", "both_fail", "incomplete")}
    both, complete = [], []
    for task, repeat in sorted({(r["task_id"], r["repetition"]) for r in results}):
        a, b = (indexed.get((task, repeat, arm)) for arm in ("qwen", "qwen_laya"))
        outcome = "incomplete"
        if a and b:
            complete.append((a, b))
            outcome = ("both_pass" if b["success"] else "qwen_only") if a["success"] else (
                "hybrid_only" if b["success"] else "both_fail")
            if outcome == "both_pass":
                both.append((a, b))
        report["pair_outcomes"][outcome] += 1
        report["pairs"].append({"task_id": task, "repetition": repeat, "outcome": outcome,
                                "qwen_termination": a["termination"] if a else None,
                                "hybrid_termination": b["termination"] if b else None})
    report["both_successful_pairs"] = len(both)
    report["complete_pairs"] = len(complete)
    report["successful_pair_coverage"] = len(both) / len(report["pairs"]) if report["pairs"] else None
    if report["pair_outcomes"]["incomplete"]:
        report["pass_rate_delta_pp"] = None
    report["paired_speedup"] = (sum(a["elapsed_ms"] for a, _ in both)
                                 / sum(b["elapsed_ms"] for _, b in both)) if both else None
    for name, pairs in (("complete", complete), ("both_successful", both)):
        count_a = sum(a["calls"]["qwen"] for a, _ in pairs)
        count_b = sum(b["calls"]["qwen"] for _, b in pairs)
        report[name + "_qwen_call_reduction"] = 1 - count_b / count_a if count_a else None
    return report
