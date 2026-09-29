"""Approved, pinned MiniWoB development A/B; never an official full-benchmark score."""

import argparse
from datetime import datetime, timezone
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import platform
import random
import time
from types import SimpleNamespace

from experiment_metrics import dump, summarize
from isolated_browser import IsolatedBrowser
from jev_reference.protocol import build_request
from miniwob_environment import FAMILIES, REVISION, MiniWobEpisode, serve_checkout, verify_checkout
from pilot_models import Laya, Qwen, audit_laya_input
from run_laya_browser import run_attempt
from verify_jev_reference import REVISION as JEV_REVISION


def input_hash(request):
    return hashlib.sha256(json.dumps(request, ensure_ascii=True, separators=(",", ":")).encode()).hexdigest()


def schedule(tasks, seed):
    pairs = []
    for index, task in enumerate(tasks):
        arms = ["qwen", "qwen_laya"] if index % 2 == 0 else ["qwen_laya", "qwen"]
        pairs.append([(task["id"], 0, arm) for arm in arms])
    random.Random(seed).shuffle(pairs)
    return [attempt for pair in pairs for attempt in pair]


def audit_tasks(config, origin):
    from huggingface_hub import hf_hub_download
    from laya.agent import Agent
    from transformers import AutoTokenizer

    cfg = config["laya"]
    tokenizer_dir = Path(hf_hub_download(cfg["model"], "tokenizer/tokenizer_config.json",
                                       revision=cfg["revision"], local_files_only=True)).parent
    agent = SimpleNamespace(tok=AutoTokenizer.from_pretrained(tokenizer_dir, local_files_only=True),
                            _check_question=Agent._check_question, _to_internal=Agent._to_internal)
    tasks, audits = [], []
    for family in config["benchmark"]["families"]:
        for seed_index, seed in enumerate(config["benchmark"]["seeds"]):
            task = {"id": family + "-s" + str(seed_index), "family": family, "seed": seed,
                    "url": origin + "/miniwob/" + family + ".html", "allowed_origins": [origin]}
            browser = IsolatedBrowser(task["url"], task["allowed_origins"])
            try:
                setup = MiniWobEpisode(seed, config["task_timeout_seconds"]).prepare(browser)
                task["goal"] = setup["goal"]
                page = browser.observe(screenshot=False)
                request = build_request(page, task["goal"], [])
                task["expected_initial_request_sha256"] = input_hash(request)
                audit = audit_laya_input(agent, request["state"], request["questions"], cfg["max_len"], cfg["head_max_len"])
                smaller = audit_laya_input(agent, request["state"], request["questions"], 1024, 512)
                audits.append({"task_id": task["id"], "setup": setup, "request": request,
                               "input_audit": audit, "comparison_1024_512": smaller,
                               "browser_version": browser.chrome.version,
                               "omitted_actions": page["omitted_actions"]})
            finally:
                browser.close()
            tasks.append(task)
    return tasks, audits


def validate_config(config):
    if config["run_status"] != "approved" or config["laya"]["selection_status"] != "approved":
        raise ValueError("Development run is not approved")
    if config["scope"] != "development_only" or config["reference_commit"] != JEV_REVISION:
        raise ValueError("Unexpected run scope or action protocol")
    benchmark = config["benchmark"]
    if benchmark["revision"] != REVISION or tuple(benchmark["families"]) != FAMILIES:
        raise ValueError("Task selection differs from the approved six families")
    if benchmark["seeds"] != ["20260929-01", "20260929-02"] or config["repetitions"] != 1:
        raise ValueError("Expected two frozen seeds, one attempt per arm each")
    if config["task_timeout_seconds"] != 120 or config["automatic_retries"] != 0 or config["laya"]["automatic_fallback"]:
        raise ValueError("Unexpected deadline, retries or fallback")
    if benchmark["mode"] != "train" or benchmark["success"] != "native_done_and_raw_reward_equals_one":
        raise ValueError("Unexpected benchmark mode or success rule")
    if any(type(config[k]) is not int or config[k] <= 0 for k in ("max_actions", "max_decisions")):
        raise ValueError("Positive action and decision limits required")


def run(config_path, checkout, output, audit_only=False):
    os.environ.update(HF_HUB_OFFLINE="1", TRANSFORMERS_OFFLINE="1")
    config = json.loads(config_path.read_text(encoding="utf-8"))
    validate_config(config)
    source = verify_checkout(checkout)
    output.mkdir(parents=True, exist_ok=False)
    start = time.perf_counter_ns()
    results, order = [], []
    with serve_checkout(checkout) as origin:
        tasks, audits = audit_tasks(config, origin)
        order = schedule(tasks, config["order_seed"])
        dump(output / "tasks.json", tasks)
        dump(output / "input-audit.json", audits)
        code = Path(__file__).parent
        dump(output / "manifest.json", {
            "scope": "input_audit_only" if audit_only else config["scope"],
            "started_utc": datetime.now(timezone.utc).isoformat(), "config": config, "benchmark": source,
            "tasks_sha256": input_hash(tasks), "config_sha256": hashlib.sha256(config_path.read_bytes()).hexdigest(),
            "order": order, "task_families": len(FAMILIES), "seeded_instances": len(tasks),
            "code_sha256": {str(p.relative_to(code)): hashlib.sha256(p.read_bytes()).hexdigest()
                            for p in sorted(code.rglob("*")) if p.suffix in {".py", ".js"}},
            "python": platform.python_version(), "platform": platform.platform(),
            "playwright": importlib.metadata.version("playwright"),
            "preparation_ms": (time.perf_counter_ns() - start) / 1e6,
        })
        if audit_only:
            print(json.dumps({"scope": "input_audit_only", "tasks": len(tasks), "model_calls": 0,
                              "output": str(output)}))
            return
        laya = None
        try:
            qwen, laya = Qwen(config["qwen"]), Laya(config["laya"])
            dump(output / "model-setup.json", laya.setup)
            indexed = {t["id"]: t for t in tasks}
            for task_id, repetition, arm in order:
                task = indexed[task_id]
                result = run_attempt(task, repetition, arm, config, output, qwen, laya,
                                     environment=MiniWobEpisode(task["seed"], config["task_timeout_seconds"]))
                results.append(result)
                print(json.dumps({k: result.get(k) for k in ("id", "success", "termination", "error_type",
                                                            "elapsed_ms", "calls", "evaluation")}), flush=True)
        except Exception as error:
            dump(output / "run-error.json", {"error_type": type(error).__name__})
            raise SystemExit("Run interrupted; partial results and schedule preserved") from None
        finally:
            if laya is not None:
                laya.hook.remove()
            report = summarize(results)
            report.update(task_families=len(FAMILIES), seeded_instances=len(tasks), scheduled_attempts=len(order),
                          not_attempted=order[len(results):], whole_run_ms=(time.perf_counter_ns() - start) / 1e6,
                          note="12 seeded instances in 6 families, not 12 independent task families; no equivalence claim")
            dump(output / "summary.json", report)
    print(json.dumps({"complete": len(results) == len(order), "summary": str(output / "summary.json")}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=Path("configs/research/laya-miniwob-development.json"))
    parser.add_argument("--checkout", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--audit-only", action="store_true")
    args = parser.parse_args()
    run(args.config, args.checkout, args.output, args.audit_only)
