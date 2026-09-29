"""Paired real-model AppWorld development rehearsal, not formal quality evidence."""

import argparse
from collections import Counter
from contextlib import contextmanager
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import platform
import random
import statistics
import time

from appworld_agent import ActionMenu, BASELINE_INSTRUCTION, GENERATOR_INSTRUCTION
from appworld_agent import parse_baseline, parse_code, state_text
from appworld_sandbox import Sandbox
from pilot_models import Laya, Qwen


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


def run_attempt(task, repetition, arm, config, data_root, run_root, qwen, laya):
    attempt_id = f"{task}-r{repetition}-{arm}"
    trace = Trace(run_root / (attempt_id + ".jsonl"), qwen.key)
    result = {"id": attempt_id, "task_id": task, "repetition": repetition, "arm": arm,
              "success": False, "evaluation": None, "termination": "setup_error"}
    world = None
    prep_start = time.perf_counter_ns()
    task_start = None
    try:
        world = Sandbox(data_root, run_root / (attempt_id + "-world"), task, attempt_id)
        if world.version != config["benchmark_version"]:
            raise ValueError("AppWorld package version mismatch")
        result["setup_ms"] = (time.perf_counter_ns() - prep_start) / 1e6
        task_start = time.perf_counter_ns()
        deadline = time.perf_counter() + config["task_timeout_seconds"]
        menu = ActionMenu(world.metadata, config["api_page_size"])
        history = []
        result["termination"] = "step_limit"
        try:
            for step in range(config["max_steps"]):
                def remaining():
                    seconds = deadline - time.perf_counter()
                    if seconds <= 0:
                        raise TimeoutError("Task budget exhausted")
                    return seconds

                remaining()
                with trace.event("framework", step, "state_and_candidates") as event:
                    state = state_text(world.metadata, history, menu)
                    candidates = menu.candidates()
                    event.update(state=state, candidates=candidates)
                if arm == "qwen":
                    with trace.event("qwen", step, "mixed") as event:
                        response = qwen.call(BASELINE_INSTRUCTION, state, event, remaining())
                        event["response"] = response
                        decision, candidate = parse_baseline(response, menu)
                        if candidate["kind"] != "generate":
                            event["purpose"] = "decision"
                    code = decision.get("code")
                else:
                    with trace.event("laya", step, "decision") as event:
                        choice = laya.choose(state, candidates, event)
                        candidate = menu.selected(choice)
                        event["selected_id"] = choice
                    code = None
                    if candidate["kind"] == "generate":
                        with trace.event("qwen", step, "generation") as event:
                            response = qwen.call(GENERATOR_INSTRUCTION, state, event, remaining())
                            event["response"] = response
                            code = parse_code(response)
                remaining()
                with trace.event("tool", step, candidate["kind"], candidate=candidate) as event:
                    timeout = min(remaining(), config["tool_timeout_seconds"])
                    kind = candidate["kind"]
                    if kind in ("generate", "finish"):
                        if kind == "finish":
                            code = "apis.supervisor.complete_task()"
                        observation = world.request("execute", timeout=timeout, code=code)
                        event["code"] = code
                    elif kind in ("docs_app", "docs_api"):
                        observation = world.request("docs", timeout=timeout, app=candidate["app"], api=candidate.get("api"))
                        menu.navigate(candidate)
                    else:
                        menu.navigate(candidate)
                        observation = {"available_actions": menu.candidates()}
                    event["observation"] = observation
                history.append({"action": candidate["id"], "code": code, "observation": observation})
                if kind in ("generate", "finish") and observation.get("task_completed"):
                    result["termination"] = "completed_signal"
                    break
        except Exception as error:
            result.update(termination="agent_error", error_type=type(error).__name__)
        # Evaluation is terminal and is never appended to model-visible history.
        with trace.event("evaluation", -1, "independent_acceptance") as event:
            result["evaluation"] = world.request("evaluate", timeout=30)
            event["result"] = result["evaluation"]
        result["timed_out"] = time.perf_counter() > deadline
        result["success"] = bool(
            result["evaluation"]["success"] and not result["timed_out"]
            and result["termination"] == "completed_signal"
        )
    except Exception as error:
        result.update(error_type=type(error).__name__)
    finally:
        now = time.perf_counter_ns()
        result["elapsed_ms"] = (now - task_start) / 1e6 if task_start is not None else None
        if task_start is None:
            result["setup_ms"] = (now - prep_start) / 1e6
        cleanup_start = time.perf_counter_ns()
        if world:
            try:
                world.close()
            except Exception as error:
                result["cleanup_error_type"] = type(error).__name__
        result["cleanup_ms"] = (time.perf_counter_ns() - cleanup_start) / 1e6
        result["calls"] = {
            name: sum(e.get("actual_calls", 0) for e in trace.events if e["component"] == name)
            for name in ("qwen", "laya")
        }
        result["qwen_by_purpose"] = dict(Counter(
            e["purpose"] for e in trace.events if e["component"] == "qwen" and e.get("actual_calls")
        ))
        result["laya_forward_attempts"] = sum(e.get("forward_attempts", 0) for e in trace.events)
        result["tool_calls"] = sum(e["component"] == "tool" for e in trace.events)
        result["execution_failures"] = sum(
            bool(e.get("observation", {}).get("execution_failed")) for e in trace.events
        )
        result["component_ms"] = {
            component: sum(e["elapsed_ms"] for e in trace.events if e["component"] == component)
            for component in ("qwen", "laya", "tool", "framework", "evaluation")
        }
        trace.write({"phase": "task_end", "result": result})
        trace.close()
    dump(run_root / (attempt_id + "-result.json"), result)
    return result


def summarize(results):
    report = {"scope": "development_rehearsal_not_formal_evidence", "arms": {}, "pairs": []}
    for arm in ("qwen", "qwen_laya"):
        rows = [row for row in results if row["arm"] == arm]
        times = [row["elapsed_ms"] for row in rows if row["elapsed_ms"] is not None]
        report["arms"][arm] = {
            "attempts": len(rows), "passed": sum(row["success"] for row in rows),
            "pass_rate": sum(row["success"] for row in rows) / len(rows) if rows else None,
            "mean_ms": statistics.mean(times) if times else None,
            "median_ms": statistics.median(times) if times else None,
            "setup_failures": sum(row["elapsed_ms"] is None for row in rows),
            "qwen_calls": sum(row["calls"]["qwen"] for row in rows),
            "laya_calls": sum(row["calls"]["laya"] for row in rows),
            "tool_calls": sum(row["tool_calls"] for row in rows),
        }
    indexed = {(r["task_id"], r["repetition"], r["arm"]): r for r in results}
    for task, repetition in sorted({(r["task_id"], r["repetition"]) for r in results}):
        a = indexed.get((task, repetition, "qwen"))
        b = indexed.get((task, repetition, "qwen_laya"))
        if a and b:
            report["pairs"].append({"task_id": task, "repetition": repetition,
                                    "qwen_passed": a["success"], "hybrid_passed": b["success"]})
    successful = [p for p in report["pairs"] if p["qwen_passed"] and p["hybrid_passed"]]
    report["both_successful_pairs"] = len(successful)
    report["paired_speedup"] = (
        sum(indexed[(p["task_id"], p["repetition"], "qwen")]["elapsed_ms"] for p in successful)
        / sum(indexed[(p["task_id"], p["repetition"], "qwen_laya")]["elapsed_ms"] for p in successful)
        if successful else None
    )
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--data-root", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--baseline-check", action="store_true",
                        help="Qwen-only pipeline check; not a paired experiment")
    args = parser.parse_args()
    config = json.loads(args.config.read_text(encoding="utf-8"))
    if not args.baseline_check and config["laya"]["selection_status"] != "approved":
        parser.error("Checkpoint selection is still awaiting user confirmation; no requests sent")
    data_root = args.data_root.resolve()
    available = set((data_root / "datasets" / (config["split"] + ".txt")).read_text().splitlines())
    if not set(config["task_ids"]).issubset(available):
        parser.error("A task is outside the frozen split")
    if (data_root / "version.txt").read_text().strip() != config["data_version"]:
        parser.error("AppWorld data version mismatch")
    args.output.mkdir(parents=True, exist_ok=False)
    run_start = time.perf_counter_ns()
    qwen = Qwen(config["qwen"])
    laya = None if args.baseline_check else Laya(config["laya"])
    order = []
    randomizer = random.Random(config["order_seed"])
    for repetition in range(config["repetitions"]):
        for task in config["task_ids"]:
            arms = ["qwen"] if args.baseline_check else ["qwen", "qwen_laya"]
            randomizer.shuffle(arms)
            order.extend((task, repetition, arm) for arm in arms)
    hashes = {path.name: hashlib.sha256(path.read_bytes()).hexdigest()
              for path in Path(__file__).parent.glob("*.py")}
    dump(args.output / "manifest.json", {
        "started_at_utc": datetime.now(timezone.utc).isoformat(), "config": config,
        "code_sha256": hashes, "config_sha256": hashlib.sha256(args.config.read_bytes()).hexdigest(),
        "order": order, "python": platform.python_version(), "platform": platform.platform(),
        "laya_setup": laya.setup if laya else None,
        "baseline_check_only": args.baseline_check,
    })
    results = []
    for task, repetition, arm in order:
        result = run_attempt(task, repetition, arm, config, data_root, args.output, qwen, laya)
        results.append(result)
        print(json.dumps(result), flush=True)
    summary = summarize(results)
    summary["baseline_check_only"] = args.baseline_check
    summary["whole_run_ms"] = (time.perf_counter_ns() - run_start) / 1e6
    dump(args.output / "summary.json", summary)
    print(json.dumps(summary), flush=True)


if __name__ == "__main__":
    main()
