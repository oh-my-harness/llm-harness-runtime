"""Run the pinned author's Suite A and implementation; add local setup and telemetry only."""

import argparse
from contextlib import contextmanager
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import threading
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[3]
REVISION = "645cf366a2ae35f1086e8c20eff48f909bb49206"
SNAPSHOT = ROOT.parent / "benchmark/models/huggingface/hub/models--cklxx--laya-browser/snapshots" / REVISION
ENGINE = ROOT.parent / "benchmark/sources/jev-ultrafast"
QWEN = "Qwen/Qwen2.5-72B-Instruct"
QWEN_BASE = "https://api.siliconflow.cn/v1"
SOURCE_HASHES = {
    "code/apps/browser_suite.py": "71359e9c9e53d0ce2fafda2fba8a98d559010f1052cccfc92a99ae018150ae7e",
    "code/jev-ultrafast.patch": "d9783e50962daf6823e49478e6ddab49c50871965a74e429408550b522270373",
    "code/apps/laya_browser.py": "3dfaba4030a1c96afcd8f5a865a66518e5edb07c6b332e0668f1519a624e6b03",
    "model.safetensors": "82844f2528b9915f8408146384409ccc99f860f56ba0e37d19d0d5775b098e52",
}


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def verify_files(snapshot, expected=SOURCE_HASHES):
    for name, digest in expected.items():
        if sha(snapshot / name) != digest:
            raise ValueError(f"Author file mismatch: {name}")


def add_usage(counts, usage):
    for source, target in (("prompt_tokens", "qwen_input_tokens"), ("completion_tokens", "qwen_output_tokens")):
        value = (usage or {}).get(source)
        if type(value) is not int or value < 0:
            counts[target] = None
        elif counts[target] is not None:
            counts[target] += value


def first_checkbox_checked(page):
    checkboxes = [a for a in page.get("actions", []) if a.get("role") == "checkbox"]
    return bool(checkboxes) and checkboxes[0].get("checked") in (True, "true")


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_http(url, timeout=45):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=1) as response:
                return json.load(response)
        except (OSError, ValueError):
            time.sleep(0.2)
    raise RuntimeError("Local service did not become ready")


class BudgetExhausted(BaseException):
    pass


def run(output, chromium=None):
    output.mkdir(parents=True, exist_ok=False)
    os.environ.update(PYTHONUTF8="1", HF_HUB_OFFLINE="1", TRANSFORMERS_OFFLINE="1")
    key = os.environ.pop("SILICONFLOW_API_KEY", None)
    if not key:
        raise ValueError("SILICONFLOW_API_KEY is required")
    expected = "1231850a0bf1a0c0341fe408ef1668dbbfdfac46"
    if subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ENGINE, text=True).strip() != expected:
        raise ValueError("Engine revision mismatch")
    subprocess.run(["git", "apply", "--reverse", "--check", str(SNAPSHOT / "code/jev-ultrafast.patch")],
                   cwd=ENGINE, check=True, capture_output=True)
    verify_files(SNAPSHOT)
    sys.path[:0] = [str(SNAPSHOT / "code/apps"), str(ENGINE)]
    os.environ.update(BH_HOME=str(output / "harness"), BU_NAME="author-" + output.name)
    for name in list(os.environ):
        if name.startswith("JEV_") or name in ("TEXT_MODEL_API_KEY", "TEXT_MODEL_EXTRA_JSON", "TEXT_MODEL_REASONING"):
            os.environ.pop(name)
    import browser_suite as suite
    import jev_ultrafast.agent as agent_module
    import jev_ultrafast.model as model_module
    from browser_harness import admin
    from laya_browser import LayaBrowser
    import torch

    cdp_port, model_port = free_port(), free_port()
    model_url = f"http://127.0.0.1:{model_port}"
    os.environ.update(BU_CDP_URL=f"http://127.0.0.1:{cdp_port}", TYPESAFE_BASE_URL=model_url,
                      TYPESAFE_API_KEY="local", TEXT_MODEL_BASE_URL=QWEN_BASE, TEXT_MODEL=QWEN,
                      TEXT_MODEL_EXTRA_JSON="{}")
    os.environ.pop("TEXT_MODEL_API_KEY", None)
    events = (output / "events.jsonl").open("x", encoding="utf-8")
    lock = threading.Lock()
    task = {"name": "setup", "purpose": None}
    counts = {"qwen_http": 0, "laya_rpc": 0, "laya_predict": 0, "laya_forward": 0,
              "tools": 0, "qwen_input_tokens": 0, "qwen_output_tokens": 0}

    def emit(component, **data):
        with lock:
            events.write(json.dumps({"utc": time.time(), "task": task["name"], "component": component,
                                     **data}, ensure_ascii=True) + "\n")
            events.flush()

    @contextmanager
    def measured(component, **data):
        start = time.perf_counter()
        try:
            yield data
            data["status"] = "ok"
        except BaseException as error:
            data.update(status="error", error_type=type(error).__name__)
            raise
        finally:
            emit(component, elapsed_ms=(time.perf_counter() - start) * 1000, **data)

    browser = None
    started = time.perf_counter()
    rows = []
    try:
        with measured("model_load"):
            lb = LayaBrowser.from_pretrained(str(SNAPSHOT), device="cuda", fast=False)
        if lb.agent.cfg["max_len"] != 1024 or lb.agent.cfg["head_max_len"] != 768:
            raise ValueError("Author defaults changed")
        def forward_hook(module, args):
            counts["laya_forward"] += 1
            device = str(next(module.parameters()).device)
            emit("laya_forward", device=device)
            if not device.startswith("cuda"):
                raise RuntimeError("Unexpected CPU inference")
        lb.agent.model.register_forward_pre_hook(forward_hook)
        predict = lb.agent.predict
        def observed_predict(state, questions, *args, **kwargs):
            counts["laya_predict"] += 1
            with measured("laya_predict", questions=len(questions)) as record:
                result = predict(state, questions, *args, **kwargs)
                record["usage"] = result.get("usage")
                return result
        lb.agent.predict = observed_predict
        systemone = lb.systemone
        def observed_systemone(body):
            counts["laya_rpc"] += 1
            with measured("laya_rpc") as record:
                result = systemone(body)
                record["answers"] = result["answers"]
                return result
        lb.systemone = observed_systemone
        threading.Thread(target=lb.serve, kwargs={"port": model_port}, daemon=True).start()
        wait_http(model_url)
        # Warm up with the same recorded request and repeat count as the author's verify.py.
        sample = json.loads((SNAPSHOT / "code/sample_request.json").read_text(encoding="utf-8"))
        for _ in range(3):
            lb.systemone(sample)
        executable = str(chromium) if chromium else subprocess.check_output(["node", "-e",
            "console.log(require('../benchmark/sources/jev-browser/node_modules/playwright').chromium.executablePath())"],
            cwd=ROOT, text=True).strip()
        browser = subprocess.Popen([executable, "--headless=new", f"--remote-debugging-port={cdp_port}",
                                    "--remote-debugging-address=127.0.0.1", "--no-first-run",
                                    f"--user-data-dir={output / 'browser-profile'}", "about:blank"],
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                   creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0)
        wait_http(os.environ["BU_CDP_URL"] + "/json/version")
        admin.ensure_daemon(wait=30)
        os.environ["TEXT_MODEL_API_KEY"] = key
        send = model_module.CLIENT.send
        def observed_send(request, *args, **kwargs):
            destination = str(request.url)
            if destination not in (model_url + "/v1/systemone", QWEN_BASE + "/chat/completions"):
                raise RuntimeError("Unexpected model destination")
            remote = destination.startswith(QWEN_BASE)
            if remote:
                if counts["qwen_http"] >= 120:
                    raise BudgetExhausted("Qwen HTTP budget exhausted")
                counts["qwen_http"] += 1
            with measured("qwen_http" if remote else "decision_http", purpose=task["purpose"]) as record:
                try:
                    response = send(request, *args, **kwargs)
                except Exception:
                    if remote:
                        add_usage(counts, None)
                    raise
                record["http_status"] = response.status_code
                if remote and response.is_success:
                    payload = response.json()
                    record.update(usage=payload.get("usage"), model=payload.get("model"))
                    add_usage(counts, payload.get("usage"))
                elif remote:
                    add_usage(counts, None)
                return response
        model_module.CLIENT.send = observed_send
        def observe_helper(name):
            original = getattr(agent_module, name)
            def call(*args, **kwargs):
                before, task["purpose"] = task["purpose"], name
                try:
                    return original(*args, **kwargs)
                finally:
                    task["purpose"] = before
            setattr(agent_module, name, call)
        for name in ("field_text", "select_option", "make_plan", "subgoal_satisfied"):
            observe_helper(name)
        act = agent_module.Browser.act
        def observed_act(self, action, *args, **kwargs):
            counts["tools"] += 1
            with measured("tool", kind=action.get("kind"), label=action.get("label")):
                return act(self, action, *args, **kwargs)
        agent_module.Browser.act = observed_act
        observe = agent_module.Browser.observe
        last_page = {}
        def observed_page(self, *args, **kwargs):
            page = observe(self, *args, **kwargs)
            last_page.clear()
            last_page.update(page)
            emit("observation", page={k: page.get(k) for k in ("url", "title", "text", "actions")})
            return page
        agent_module.Browser.observe = observed_page
        manifest = {"source": REVISION, "engine": expected, "suite": "A", "repeats": 1,
                    "tasks": [row[0] for row in suite.TASKS], "qwen": QWEN, "qwen_http_limit": 120,
                    "max_len": 1024, "head_max_len": 768, "fast": False, "device": str(lb.agent.device),
                    "python": sys.version, "torch": torch.__version__,
                    "packages": {n: importlib.metadata.version(n) for n in ("laya", "transformers", "browser-harness")},
                    "author_files_sha256": {p: sha(SNAPSHOT / p) for p in SOURCE_HASHES},
                    "runner_sha256": sha(Path(__file__)), "chromium": executable,
                    "enabled": {"undo_guard": True, "select_llm": True, "safe": False,
                                "planner": False, "escalate": False, "verify_done": False, "macros": False},
                    "warmup_counts": dict(counts)}
        (output / "manifest.json").write_text(json.dumps(manifest, indent=2), encoding="utf-8")
        for original_task in suite.TASKS:
            task["name"] = original_task[0]
            before = dict(counts)
            attempt_started = time.perf_counter()
            ok, steps, status, wall, final = suite.run(*original_task)
            row = {"task": task["name"], "pass": ok, "steps": steps, "status": status, "wall_seconds": wall,
                   "attempt_wall_seconds": time.perf_counter() - attempt_started,
                   "final_url": final, "counts": {k: None if v is None or before[k] is None else v - before[k]
                                                   for k, v in counts.items()}}
            rows.append(row)
            if task["name"] == "internet-checkbox":
                row["independent_first_checkbox_checked"] = first_checkbox_checked(last_page)
            with (output / "attempts.jsonl").open("a", encoding="utf-8") as handle:
                handle.write(json.dumps(row) + "\n")
            print(json.dumps(row), flush=True)
        return rows
    finally:
        task["name"] = "cleanup"
        os.environ.pop("TEXT_MODEL_API_KEY", None)
        if browser is not None:
            try:
                from browser_harness.helpers import cdp
                cdp("Browser.close")
            except Exception:
                pass
            try:
                browser.wait(timeout=8)
            except subprocess.TimeoutExpired:
                browser.terminate()
                browser.wait(timeout=8)
            try:
                admin.restart_daemon(name=os.environ["BU_NAME"])
            except Exception as error:
                emit("cleanup_error", error_type=type(error).__name__)
        report = {"completed": len(rows), "unstarted": len(suite.TASKS) - len(rows),
                  "passed": sum(row["pass"] for row in rows), "counts_including_warmup": counts,
                  "session_wall_seconds": time.perf_counter() - started, "rows": rows}
        (output / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
        events.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--chromium", type=Path)
    args = parser.parse_args()
    run(args.output.resolve(), args.chromium)
