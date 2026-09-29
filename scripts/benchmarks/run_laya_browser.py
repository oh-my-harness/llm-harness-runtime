"""Jev-aligned A/B development runner. No built-in benchmark or simulated model arm."""

import argparse
from collections import Counter
from datetime import datetime, timezone
import hashlib
import importlib.metadata
import json
from pathlib import Path
import platform
import random
import time

from browser_policy import Policy
from experiment_metrics import Trace, dump, summarize
from isolated_browser import IsolatedBrowser, evaluate, origin, validate_acceptance
from jev_reference.browser import StalePage
from jev_reference.protocol import build_request
from pilot_models import Laya, Qwen
from verify_jev_reference import REVISION


def run_attempt(task, repetition, arm, config, run_root, qwen, laya, *, environment=None):
    attempt_id = f"{task['id']}-r{repetition}-{arm}"
    trace = Trace(run_root / (attempt_id + ".jsonl"), qwen.key)
    result = {"id": attempt_id, "task_id": task["id"], "repetition": repetition, "arm": arm,
              "success": False, "evaluation": None, "termination": "setup_error", "stale_decisions": 0}
    if environment is not None:
        result.update(task_family=task["family"], seed=task["seed"])
    browser, task_start = None, None
    start = time.perf_counter_ns()
    try:
        browser = IsolatedBrowser(task["url"], task["allowed_origins"])
        goal = task["goal"]
        if environment is not None:
            with trace.event("setup", -1, "benchmark_reset") as event:
                result["environment_setup"] = environment.prepare(browser)
                event["result"] = result["environment_setup"]
                goal = result["environment_setup"]["goal"]
        result["goal"] = goal
        result["setup_ms"] = (time.perf_counter_ns() - start) / 1e6
        result["browser_version"] = browser.chrome.version
        result["setup_agent_cdp_calls"] = dict(browser.session.calls)
        task_start = time.perf_counter_ns()
        deadline = time.perf_counter() + config["task_timeout_seconds"]
        policy = Policy(arm, qwen, laya, trace)
        history = []

        def remaining():
            value = deadline - time.perf_counter()
            if value <= 0:
                raise TimeoutError("Task deadline exhausted")
            return value

        def observe(step):
            remaining()
            with trace.event("tool", step, "observe") as event:
                page = browser.observe(screenshot=False)
                event["fingerprint"] = page["fingerprint"]
                return page

        def environment_done(step):
            if environment is None:
                return False
            with trace.event("environment", step, "terminal_status_only") as event:
                event["done"] = environment.terminated(browser)
                return event["done"]

        try:
            page = observe(-1)
            initial = build_request(page, goal, [])
            result["initial_request_sha256"] = hashlib.sha256(
                json.dumps(initial, ensure_ascii=True, separators=(",", ":")).encode()).hexdigest()
            if (task.get("expected_initial_request_sha256") is not None
                    and result["initial_request_sha256"] != task["expected_initial_request_sha256"]):
                raise ValueError("Initial policy input differs from the frozen task observation")
            result["termination"] = "decision_limit"
            for step in range(config["max_decisions"]):
                remaining()
                if environment_done(step):
                    result["termination"] = "environment_done"
                    break
                try:
                    with trace.event("tool", step, "fresh_before_decision"):
                        fresh = browser.fresh(page)
                    if not fresh:
                        page = observe(step)
                    decision, action = policy.decide(page, goal, history, step, remaining)
                    remaining()
                    operation = decision["operation"]
                    if operation in {"DONE", "BLOCKED"}:
                        with trace.event("tool", step, "fresh_terminal_signal"):
                            if not browser.fresh(page):
                                raise StalePage("Terminal choice belongs to a stale page")
                        result["termination"] = operation.lower()
                        break
                    if len(history) >= config["max_actions"]:
                        result["termination"] = "action_limit"
                        break
                    value = None
                    if operation == "TYPE_TEXT":
                        with trace.event("tool", step, "fresh_before_text"):
                            if not browser.fresh(page):
                                raise StalePage("Text target belongs to a stale page")
                        value = policy.text(decision, action, page, goal, history, step, remaining)
                    remaining()
                    with trace.event("tool", step, "act", decision=decision, action=action, text=value) as event:
                        event["result"] = browser.act(action, page, text=value)
                    # Record a successful mutation before observing again. Never replay it on observation failure.
                    history.append({"action": action["label"], "kind": action["kind"], "text": value,
                                    "operation": operation, "target": decision["target"], "page_changed": None})
                    policy.executed()
                    if environment_done(step):
                        result["termination"] = "environment_done"
                        break
                    old_fingerprint = page["fingerprint"]
                    page = observe(step)
                    history[-1]["page_changed"] = page["fingerprint"] != old_fingerprint
                    last = history[-3:]
                    if len(last) == 3 and all(h["page_changed"] is False and h["kind"] != "wait" for h in last):
                        result["termination"] = "no_progress"
                        break
                except StalePage:
                    result["stale_decisions"] += 1
                    page = observe(step)
        except Exception as error:
            result.update(termination="agent_error", error_type=type(error).__name__)
        with trace.event("evaluation", -1, "independent_acceptance") as event:
            # The checker is terminal. Its fields and outcome never enter policy state or history.
            result["evaluation"] = (environment.evaluate(browser) if environment is not None
                                    else evaluate(browser.page, task["acceptance"]))
            event["result"] = result["evaluation"]
        result["timed_out"] = time.perf_counter() > deadline
        terminal = result["termination"] == "done" or (environment is not None and result["termination"] == "environment_done")
        result["success"] = bool(terminal and result["evaluation"]["success"]
                                 and not result["timed_out"])
        result["actions_executed"] = len(history)
    except Exception as error:
        result["error_type"] = type(error).__name__
    finally:
        now = time.perf_counter_ns()
        result["elapsed_ms"] = (now - task_start) / 1e6 if task_start is not None else None
        if task_start is None:
            result["setup_ms"] = (now - start) / 1e6
        cleanup_start = time.perf_counter_ns()
        if browser:
            result["agent_cdp_calls"] = dict(browser.session.calls)
            result["agent_cdp_failures"] = browser.session.failures
            result["agent_cdp_ms"] = browser.session.elapsed_ms
            result["blocked_browser_requests"] = browser.blocked_requests
            try:
                browser.close()
            except Exception as error:
                result["cleanup_error_type"] = type(error).__name__
        result["cleanup_ms"] = (time.perf_counter_ns() - cleanup_start) / 1e6
        result["whole_attempt_ms"] = (time.perf_counter_ns() - start) / 1e6
        result["calls"] = {name: sum(e.get("actual_calls", 0) for e in trace.events if e["component"] == name)
                           for name in ("qwen", "laya")}
        result["qwen_by_purpose"] = dict(Counter(
            e["purpose"] for e in trace.events if e["component"] == "qwen" and e.get("actual_calls")))
        result["laya_forward_attempts"] = sum(e.get("forward_attempts", 0) for e in trace.events)
        result["decision_questions"] = sum(e.get("decision_questions", 0) for e in trace.events
                                            if e.get("actual_calls"))
        result["retry_calls"] = sum(e.get("actual_calls", 0) for e in trace.events if e.get("retry_index", 0) > 0)
        result["tool_calls"] = sum(e["component"] == "tool" for e in trace.events)
        result["execution_failures"] = sum(e["purpose"] == "act" and e["status"] == "error" for e in trace.events)
        result["component_ms"] = {name: sum(e["elapsed_ms"] for e in trace.events if e["component"] == name)
                                  for name in ("qwen", "laya", "tool", "framework", "environment", "evaluation")}
        trace.write({"phase": "task_end", "result": result})
        trace.close()
    dump(run_root / (attempt_id + "-result.json"), result)
    return result


def validate_tasks(tasks):
    if not isinstance(tasks, list) or not tasks:
        raise ValueError("Provide a reviewed nonempty task manifest; there is no default benchmark")
    seen = set()
    for task in tasks:
        key = task["id"]
        if (not isinstance(key, str) or not key or len(key) > 80
                or any(c not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_" for c in key)
                or key in seen):
            raise ValueError("Task IDs must be unique safe filename components")
        seen.add(key)
        if not isinstance(task["goal"], str) or not task["goal"].strip():
            raise ValueError("Task has no goal")
        if origin(task["url"]) not in {origin(s) for s in task["allowed_origins"]}:
            raise ValueError("Task URL is outside its allowlist")
        if task["reset_policy"] != "fresh_browser_context_client_state_only":
            raise ValueError("Server-side state reset requires a benchmark-specific reviewed adapter")
        validate_acceptance(task["acceptance"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--tasks", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--validate-only", action="store_true")
    args = parser.parse_args()
    config = json.loads(args.config.read_text(encoding="utf-8"))
    tasks = json.loads(args.tasks.read_text(encoding="utf-8"))
    validate_tasks(tasks)
    if args.validate_only:
        print(json.dumps({"task_manifest_valid": True, "tasks": len(tasks), "model_calls": 0,
                          "run_status": config["run_status"]}))
        return
    if config["run_status"] != "approved" or config["laya"]["selection_status"] != "approved":
        parser.error("Task/checkpoint configuration is not approved; no models loaded or API calls sent")
    if config["scope"] != "development_only" or not args.output:
        parser.error("Only reviewed development runs with a new output directory are supported")
    if config["reference_commit"] != REVISION:
        parser.error("Configuration does not match the implemented upstream revision")
    if config["automatic_retries"] != 0 or config["laya"]["automatic_fallback"] is not False:
        parser.error("This protocol supports no automatic model retries or model fallback")
    if any(type(config[name]) is not int or config[name] <= 0
           for name in ("repetitions", "max_actions", "max_decisions", "task_timeout_seconds")):
        parser.error("Budgets and repetitions must be positive integers")
    args.output.mkdir(parents=True, exist_ok=False)
    dump(args.output / "tasks.json", tasks)
    run_start = time.perf_counter_ns()
    order, rng = [], random.Random(config["order_seed"])
    for repetition in range(config["repetitions"]):
        for task in tasks:
            arms = ["qwen", "qwen_laya"]
            rng.shuffle(arms)
            order.extend((task["id"], repetition, arm) for arm in arms)
    source = Path(__file__).parent
    dump(args.output / "manifest.json", {
        "started_at_utc": datetime.now(timezone.utc).isoformat(), "config": config,
        "tasks_sha256": hashlib.sha256(args.tasks.read_bytes()).hexdigest(),
        "config_sha256": hashlib.sha256(args.config.read_bytes()).hexdigest(), "order": order,
        "code_sha256": {str(p.relative_to(source)): hashlib.sha256(p.read_bytes()).hexdigest()
                        for p in sorted(source.rglob("*")) if p.suffix in {".py", ".js"}},
        "python": platform.python_version(), "platform": platform.platform(),
        "playwright": importlib.metadata.version("playwright"),
    })
    results = []
    try:
        qwen, laya = Qwen(config["qwen"]), Laya(config["laya"])
        dump(args.output / "model-setup.json", laya.setup)
        indexed = {task["id"]: task for task in tasks}
        for task_id, repetition, arm in order:
            result = run_attempt(indexed[task_id], repetition, arm, config, args.output, qwen, laya)
            results.append(result)
            print(json.dumps(result), flush=True)
    except Exception as error:
        dump(args.output / "run-error.json", {"error_type": type(error).__name__})
        raise SystemExit("Run interrupted; error type and incomplete schedule saved") from None
    finally:
        report = summarize(results)
        report["scheduled_attempts"] = len(order)
        report["not_attempted"] = order[len(results):]
        report["whole_run_ms"] = (time.perf_counter_ns() - run_start) / 1e6
        dump(args.output / "summary.json", report)


if __name__ == "__main__":
    main()
