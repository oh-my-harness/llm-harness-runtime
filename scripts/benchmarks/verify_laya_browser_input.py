"""Real, offline Laya forwards on captured fixture states; NOT an agent benchmark."""

import argparse
from collections import Counter
from contextlib import contextmanager
from copy import deepcopy
from datetime import datetime, timezone
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.metadata
import json
import os
from pathlib import Path
import platform
import subprocess
import threading
import time

from browser_policy import parse_laya
from isolated_browser import IsolatedBrowser, evaluate
from jev_reference.protocol import build_request
from pilot_models import InputBudgetError, Laya


ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "scripts/tests/fixtures/browser_contract.html"
BUDGETS = ((512, 192), (1024, 512), (2048, 512))
REPETITIONS = 3
GOAL = "Enter Demo, select Gold, save, and stop."


def digest(value):
    return hashlib.sha256(json.dumps(value, ensure_ascii=True, separators=(",", ":")).encode()).hexdigest()


@contextmanager
def fixture_server():
    content = FIXTURE.read_bytes()

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(content)))
            self.end_headers()
            self.wfile.write(content)

        def log_message(self, *_args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield "http://127.0.0.1:" + str(server.server_port)
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


def capture_states():
    # Scripted transitions collect test inputs only. They never count as model actions or successes.
    with fixture_server() as origin:
        browser = IsolatedBrowser(origin + "/", [origin])
        try:
            states, history = [], []
            page = browser.observe(screenshot=False)
            for phase, transition in (
                ("empty", ("fill", "Name", "Demo")),
                ("name_filled", ("select", "Tier \u2192 Gold", None)),
                ("form_ready", ("click", "Save", None)),
                ("saved", None),
            ):
                request = build_request(page, GOAL, history)
                states.append({"phase": phase, "page": page, "request": request,
                               "request_sha256": digest(request)})
                if transition:
                    kind, label, value = transition
                    action = next(a for a in page["actions"] if a["kind"] == kind and a["label"] == label)
                    browser.act(action, page, text=value)
                    after = browser.observe(screenshot=False)
                    history.append({"action": label, "kind": kind, "text": value,
                                    "page_changed": after["fingerprint"] != page["fingerprint"]})
                    page = after
            check = evaluate(browser.page, {"visible_text": ["Saved Demo / Gold"], "fields": [
                {"selector": "#name", "value": "Demo"}, {"selector": "#tier", "value": "gold"},
            ]})
            if not check["success"]:
                raise ValueError("Fixture state preparation failed")
            return states, {"browser": browser.chrome.version, "fixture_preparation_check": check,
                            "scripted_setup_actions": 3, "model_executed_actions": 0}
        finally:
            browser.close()


def expected_sequences(agent, request, max_len, head_max_len):
    questions = request["questions"]
    items = agent._encode_state(request["state"], list(questions),
                                {k: agent._to_internal(v) for k, v in questions.items()},
                                max_len=max_len, head_max_len=head_max_len)
    return [{"head": name, "tokens": len(item["ids"]), "sha256": digest(item["ids"])}
            for name, item in zip(questions, items)]


def verify_sdk_source(checkout, revision):
    installed = Path(importlib.metadata.distribution("laya").locate_file("laya"))
    result = {}
    for name in ("agent.py", "common.py", "__init__.py"):
        upstream = subprocess.run(["git", "show", revision + ":laya/" + name], cwd=checkout,
                                  check=True, capture_output=True).stdout.replace(b"\r\n", b"\n")
        local = (installed / name).read_bytes().replace(b"\r\n", b"\n")
        result[name] = {"upstream_sha256": hashlib.sha256(upstream).hexdigest(),
                        "installed_sha256": hashlib.sha256(local).hexdigest(), "match": local == upstream}
    if not all(item["match"] for item in result.values()):
        raise ValueError("Installed SDK source differs from pinned upstream")
    return result


def sdk_crosschecks(model, states, observed, probes, record):
    # Bypass our adapter, split heads, and disable autocast separately. Never change the request.
    reference = {r["phase"]: r for r in probes if r["max_len"] == 1024 and r["status"] == "returned"}
    for captured in states:
        request = captured["request"]
        for mode in ("raw_sdk_batch_bf16", "raw_sdk_single_heads_bf16", "raw_sdk_batch_fp32"):
            event = {"kind": "sdk_crosscheck", "mode": mode, "phase": captured["phase"],
                     "request_sha256": digest(request), "max_len": 1024, "head_max_len": 512,
                     "actual_calls": 0, "retry_index": 0}
            groups = ([{name: question} for name, question in request["questions"].items()]
                      if mode == "raw_sdk_single_heads_bf16" else [request["questions"]])
            before, amp = len(observed), model.agent.amp_enabled
            if mode == "raw_sdk_batch_fp32":
                model.agent.amp_enabled = False
            model.torch.cuda.synchronize()
            start = time.perf_counter_ns()
            try:
                answers = {}
                for questions in groups:
                    event["actual_calls"] += 1
                    response = model.agent.predict(request["state"], questions, max_len=1024, head_max_len=512)
                    answers.update(response["answers"])
                baseline = reference[captured["phase"]]["answers"]
                event.update(status="returned", answers=answers,
                             same_choices_as_adapter=all(a["choice"] == baseline[h]["choice"]
                                                         for h, a in answers.items()),
                             max_probability_abs_delta=max(abs(p - baseline[h]["probabilities"][k])
                                                           for h, a in answers.items()
                                                           for k, p in a["probabilities"].items()),
                             decision=parse_laya(answers, request, captured["page"])[0])
            except Exception as error:
                event.update(status="error", error_type=type(error).__name__)
            finally:
                model.torch.cuda.synchronize()
                model.agent.amp_enabled = amp
                event["elapsed_ms_including_instrumentation"] = (time.perf_counter_ns() - start) / 1e6
                event["observed_forwards"] = observed[before:]
                event["forward_attempts"] = len(observed) - before
                event["cpu_fallback_count"] = model.agent.cpu_fallback_count
                expected = expected_sequences(model.agent, request, 1024, 512)
                actual_hashes = [s["sha256"] for f in observed[before:] for s in f["sequences"]]
                event["forward_inputs_match_sdk_sequences"] = (
                    Counter(actual_hashes) == Counter(s["sha256"] for s in expected))
                record(event)
            if digest(request) != captured["request_sha256"] or model.agent.cpu_fallback_count:
                raise RuntimeError("Crosscheck changed input or execution device")


def run(output, checkout, replay_events=None):
    # No provider credential is needed, and model/tokenizer downloads are disabled.
    os.environ.update(HF_HUB_OFFLINE="1", TRANSFORMERS_OFFLINE="1")
    start = time.perf_counter_ns()
    output.mkdir(parents=True, exist_ok=False)
    records = []
    with (output / "events.jsonl").open("x", encoding="utf-8") as log:
        def record(event):
            records.append(event)
            log.write(json.dumps(event, ensure_ascii=True) + "\n")
            log.flush()

        config = json.loads((ROOT / "configs/research/laya-browser-development.json").read_text())
        record({"kind": "manifest", "scope": "input_verification_only_not_agent_benchmark",
                "started_utc": datetime.now(timezone.utc).isoformat(), "budgets": BUDGETS,
                "repetitions_per_state_budget": REPETITIONS, "source_config": config,
                "fixture_sha256": hashlib.sha256(FIXTURE.read_bytes()).hexdigest(),
                "script_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                "python": platform.python_version(), "platform": platform.platform(),
                "paid_api_calls": 0, "generation_calls": 0, "goal": GOAL,
                "sdk_source_verification": verify_sdk_source(checkout, config["laya"]["code_revision"])})
        if replay_events:
            source = [json.loads(line) for line in replay_events.read_text(encoding="utf-8").splitlines()]
            capture = next(item for item in source if item["kind"] == "captured_states")
            states = capture["states"]
            browser_info = {"replayed_real_browser_observations": True,
                            "source_events_sha256": hashlib.sha256(replay_events.read_bytes()).hexdigest(),
                            "browser": capture["browser"], "scripted_setup_actions_this_run": 0}
            if any(digest(s["request"]) != s["request_sha256"] for s in states):
                raise ValueError("Captured input hash mismatch")
        else:
            states, browser_info = capture_states()
        record({"kind": "captured_states", **browser_info, "states": states})
        # User-authorized input verification is separate from approval of a benchmark task set.
        model = Laya(deepcopy(config["laya"]), input_verification_only=True)
        record({"kind": "setup", **model.setup, "packages": {
            name: importlib.metadata.version(name) for name in ("laya", "torch", "transformers", "playwright")},
            "encoder_max_position_embeddings": model.agent.model.encoder.config.max_position_embeddings})
        observed = []

        def observe_forward(module, args):
            ids, masks = args[0].detach().cpu().tolist(), args[1].detach().cpu().tolist()
            observed.append({"device": str(next(module.parameters()).device), "shape": list(args[0].shape),
                             "sequences": [{"tokens": sum(mask),
                                            "sha256": digest([token for token, keep in zip(row, mask) if keep])}
                                           for row, mask in zip(ids, masks)]})

        hook = model.agent.model.register_forward_pre_hook(observe_forward)
        try:
            # Alternate the two valid budgets to avoid assigning all first-call overhead to one.
            for repeat in range(REPETITIONS):
                budgets = BUDGETS if repeat % 2 == 0 else (BUDGETS[0], BUDGETS[2], BUDGETS[1])
                for max_len, head_max_len in budgets:
                    model.config.update(max_len=max_len, head_max_len=head_max_len)
                    for captured in states:
                        request, page = captured["request"], captured["page"]
                        expected = expected_sequences(model.agent, request, max_len, head_max_len)
                        event = {"kind": "decision_probe", "repeat": repeat, "phase": captured["phase"],
                                 "max_len": max_len, "head_max_len": head_max_len,
                                 "request_sha256": digest(request), "expected_sequences": expected}
                        before = len(observed)
                        model.torch.cuda.reset_peak_memory_stats()
                        call_start = time.perf_counter_ns()
                        try:
                            answers = model.predict(request["state"], request["questions"], event)
                            decision, action = parse_laya(answers, request, page)
                            event.update(status="returned", decision=decision,
                                         selected_action_label=action["label"] if action else None)
                        except InputBudgetError:
                            event.update(status="input_rejected", error_type="InputBudgetError")
                        except Exception as error:
                            event.update(status="error", error_type=type(error).__name__)
                        finally:
                            model.torch.cuda.synchronize()
                            event["elapsed_ms_including_audit_and_instrumentation"] = (time.perf_counter_ns() - call_start) / 1e6
                            event["peak_allocated_mib"] = model.torch.cuda.max_memory_allocated() / 2**20
                            event["observed_forwards"] = observed[before:]
                            actual_hashes = [seq["sha256"] for forward in observed[before:] for seq in forward["sequences"]]
                            event["forward_inputs_match_sdk_sequences"] = (
                                Counter(actual_hashes) == Counter(seq["sha256"] for seq in expected)
                                if event.get("actual_calls") else None)
                            record(event)
                        if digest(request) != captured["request_sha256"]:
                            raise ValueError("Captured request was mutated")
                        if event["forward_inputs_match_sdk_sequences"] is False or model.agent.cpu_fallback_count:
                            raise RuntimeError("Unexpected inference execution path")
            probes = [r for r in records if r["kind"] == "decision_probe"]
            sdk_crosschecks(model, states, observed, probes, record)
        finally:
            hook.remove()
            model.hook.remove()
        probes = [r for r in records if r["kind"] == "decision_probe"]
        crosschecks = [r for r in records if r["kind"] == "sdk_crosscheck"]
        summary = {"kind": "summary", "scope": "input_verification_only_not_agent_benchmark",
                   "decision_probe_counts": dict(Counter(r["status"] for r in probes)),
                   "actual_decision_calls": sum(r["actual_calls"] for r in probes),
                   "actual_decision_forwards": sum(r["forward_attempts"] for r in probes),
                   "warmup_calls": len(model.setup["warmup"]),
                   "warmup_forwards": sum(r["forward_attempts"] for r in model.setup["warmup"]),
                   "crosscheck_calls": sum(r["actual_calls"] for r in crosschecks),
                   "crosscheck_forwards": sum(r["forward_attempts"] for r in crosschecks),
                   "crosschecks_same_choices": sum(r.get("same_choices_as_adapter", False) for r in crosschecks),
                   "crosscheck_errors": sum(r["status"] != "returned" for r in crosschecks),
                   "paid_api_calls": 0, "model_executed_actions": 0,
                   "task_pass_rate": None, "speedup": None,
                   "total_elapsed_ms": (time.perf_counter_ns() - start) / 1e6}
        record(summary)
    (output / "summary.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
    print(json.dumps(summary))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--laya-checkout", type=Path, required=True)
    parser.add_argument("--replay-events", type=Path)
    args = parser.parse_args()
    run(args.output, args.laya_checkout, args.replay_events)
