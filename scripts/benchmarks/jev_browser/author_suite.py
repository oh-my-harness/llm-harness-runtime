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
import traceback
import urllib.request
from urllib.parse import urlparse

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


def common_qwen_body(body):
    return {**body, "temperature": 0, "top_p": 1, "max_tokens": 1024,
            "response_format": {"type": "json_object"}}


def controlled_qwen_post(client, url, key, body, wait):
    import httpx
    for attempt in range(3):
        try:
            response = client.post(url, json=common_qwen_body(body), headers={"Authorization": f"Bearer {key}"})
        except httpx.HTTPError:
            raise RuntimeError("Model connection failed; no action executed.") from None
        if response.status_code in (429, 503, 529) and attempt < 2:
            delay = (30, 60)[attempt] if response.status_code == 429 else .5 * 2**attempt
            try:
                delay = max(delay, min(120, float(response.headers.get("retry-after", 0))))
            except (TypeError, ValueError):
                pass
            wait(delay, response.status_code)
            continue
        if response.is_error:
            raise RuntimeError(f"Model provider returned HTTP {response.status_code}; no action executed.")
        return response.json()


def validate_run_options(paired, repeats, controlled, proxy):
    if repeats not in (1, 2, 3) or (repeats != 1 and not paired):
        raise ValueError("Use one to three paired repetitions")
    if controlled and (not paired or not proxy):
        raise ValueError("Controlled runs require paired mode and an explicit local proxy")
    if proxy:
        parsed = urlparse(proxy)
        if (parsed.scheme != "http" or parsed.hostname not in ("127.0.0.1", "localhost")
                or not parsed.port or parsed.username or parsed.password or parsed.path not in ("", "/")
                or parsed.query or parsed.fragment):
            raise ValueError("Proxy must be a local HTTP endpoint without credentials")


def run(output, chromium=None, paired=False, repeats=1, controlled=False, proxy=None, qwen_http_limit=None):
    validate_run_options(paired, repeats, controlled, proxy)
    qwen_limit = qwen_http_limit if qwen_http_limit is not None else 120 * repeats
    if not 1 <= qwen_limit <= 360:
        raise ValueError("Qwen HTTP limit must be between 1 and 360")
    output.mkdir(parents=True, exist_ok=False)
    (output / "runner-source.py").write_bytes(Path(__file__).read_bytes())
    os.environ.update(PYTHONUTF8="1", HF_HUB_OFFLINE="1", TRANSFORMERS_OFFLINE="1")
    key = os.environ.pop("SILICONFLOW_API_KEY", None)
    if not key:
        raise ValueError("SILICONFLOW_API_KEY is required")
    if proxy:
        os.environ.update(HTTP_PROXY=proxy, HTTPS_PROXY=proxy, NO_PROXY="localhost,127.0.0.1,::1")
        os.environ.pop("ALL_PROXY", None)
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
    if proxy:
        import httpx
        model_module.CLIENT.close()
        model_module.CLIENT = httpx.Client(http2=True, timeout=25, trust_env=False,
            mounts={"https://api.siliconflow.cn": httpx.HTTPTransport(proxy=proxy, http2=True, trust_env=False)})
    if controlled:
        original_post = model_module.post_json
        def provider_wait(delay, status):
            with measured("provider_backoff", http_status=status, requested_seconds=delay):
                time.sleep(delay)
        def controlled_post(url, token, body):
            if url.startswith(QWEN_BASE):
                return controlled_qwen_post(model_module.CLIENT, url, token, body, provider_wait)
            return original_post(url, token, body)
        model_module.post_json = controlled_post
    if paired:
        import author_control
        (output / "control-source.py").write_bytes(Path(author_control.__file__).read_bytes())

    cdp_port, model_port = free_port(), free_port()
    model_url = f"http://127.0.0.1:{model_port}"
    os.environ.update(BU_CDP_URL=f"http://127.0.0.1:{cdp_port}", TYPESAFE_BASE_URL=model_url,
                      TYPESAFE_API_KEY="local", TEXT_MODEL_BASE_URL=QWEN_BASE, TEXT_MODEL=QWEN,
                      TEXT_MODEL_EXTRA_JSON="{}")
    os.environ.pop("TEXT_MODEL_API_KEY", None)
    events = (output / "events.jsonl").open("x", encoding="utf-8")
    lock = threading.Lock()
    task = {"name": "setup", "purpose": None, "arm": None, "repeat": None, "deadline": float("inf"), "initial": None,
            "tokens": {"qwen_input_tokens": 0, "qwen_output_tokens": 0}}
    counts = {"qwen_http": 0, "laya_rpc": 0, "laya_predict": 0, "laya_forward": 0,
              "tools": 0, "qwen_input_tokens": 0, "qwen_output_tokens": 0}

    def emit(component, **data):
        with lock:
            line = json.dumps({"utc": time.time(), "task": task["name"], "arm": task["arm"], "repeat": task["repeat"],
                               "component": component, **data}, ensure_ascii=True)
            events.write(line.replace(key, "[REDACTED]") + "\n")
            events.flush()

    @contextmanager
    def measured(component, **data):
        start = time.perf_counter()
        try:
            yield data
            data["status"] = "ok"
        except BaseException as error:
            data.update(status="error", error_type=type(error).__name__)
            data["frames"] = [{"file": f.filename, "line": f.lineno, "function": f.name}
                              for f in traceback.extract_tb(error.__traceback__)]
            raise
        finally:
            emit(component, elapsed_ms=(time.perf_counter() - start) * 1000, **data)

    browser = None
    started = time.perf_counter()
    rows = []
    planned = len(suite.TASKS) * (2 if paired else 1) * repeats
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
        proxy_args = [f"--proxy-server={proxy}", "--proxy-bypass-list=localhost;127.0.0.1;[::1]"] if proxy else []
        browser = subprocess.Popen([executable, *proxy_args, "--headless=new", f"--remote-debugging-port={cdp_port}",
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
                if counts["qwen_http"] >= qwen_limit:
                    raise BudgetExhausted("Qwen HTTP budget exhausted")
                counts["qwen_http"] += 1
            with measured("qwen_http" if remote else "decision_http", purpose=task["purpose"]) as record:
                if remote:
                    record["request_body"] = json.loads(request.content)
                    record["network_route"] = proxy or "inherited"
                try:
                    response = send(request, *args, **kwargs)
                except Exception:
                    if remote:
                        add_usage(counts, None)
                        add_usage(task["tokens"], None)
                    raise
                record["http_status"] = response.status_code
                if remote:
                    record["response_headers"] = {k: v for k, v in response.headers.items()
                                                  if k.lower() in ("retry-after", "x-siliconcloud-trace-id")
                                                  or k.lower().startswith("x-ratelimit-")}
                if remote and response.is_success:
                    payload = response.json()
                    record.update(usage=payload.get("usage"), model=payload.get("model"))
                    record["response_content"] = [choice.get("message", {}).get("content") for choice in payload.get("choices", [])]
                    add_usage(counts, payload.get("usage"))
                    add_usage(task["tokens"], payload.get("usage"))
                elif remote:
                    add_usage(counts, None)
                    add_usage(task["tokens"], None)
                    record["response_error_body"] = response.text
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
        browser_init = agent_module.Browser.__init__
        def observed_browser_init(self, *args, **kwargs):
            with measured("browser_start"):
                browser_init(self, *args, **kwargs)
        agent_module.Browser.__init__ = observed_browser_init
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
            if task["initial"] is None:
                task["initial"] = hashlib.sha256(json.dumps({k: page.get(k) for k in ("url", "title", "text", "actions")},
                                                            sort_keys=True).encode()).hexdigest()
            emit("observation", page={k: page.get(k) for k in ("url", "title", "text", "actions")})
            return page
        agent_module.Browser.observe = observed_page
        context = {"id": None}
        if paired:
            suite.Agent = author_control.install(agent_module, model_module, task, emit)
            import jev_ultrafast.browser as browser_module
            raw_cdp = browser_module.cdp
            def isolated_cdp(method, **params):
                if method == "Target.createTarget" and context["id"] is not None:
                    params["browserContextId"] = context["id"]
                return raw_cdp(method, **params)
            browser_module.cdp = isolated_cdp
        schedule = [(t, arm, rep) for rep in range(1, repeats + 1)
                    for t, arm in (author_control.schedule(suite.TASKS, rep - 1) if paired else [(t, "B") for t in suite.TASKS])]
        manifest = {"source": REVISION, "engine": expected, "suite": "A", "repeats": repeats,
                    "paired": paired, "order": [{"task": t[0], "arm": arm, "repeat": rep} for t, arm, rep in schedule],
                    "controlled": controlled, "common_qwen_parameters": common_qwen_body({}) if controlled else None,
                    "network": {"browser_proxy": proxy, "qwen_proxy": proxy, "local_services": "direct" if proxy else "inherited",
                                "httpx_trust_env": not bool(proxy), "qwen_timeout_seconds": 25},
                    "qwen_429_backoff_seconds": [30, 60] if controlled else [.5, 1],
                    "shared_null_guard_fix": paired, "isolated_browser_contexts": paired,
                    "cooperative_attempt_seconds": 180 if paired else None,
                    "tasks": [row[0] for row in suite.TASKS], "qwen": QWEN, "qwen_http_limit": qwen_limit,
                    "max_len": 1024, "head_max_len": 768, "fast": False, "device": str(lb.agent.device),
                    "python": sys.version, "torch": torch.__version__,
                    "packages": {n: importlib.metadata.version(n) for n in ("laya", "transformers", "browser-harness")},
                    "author_files_sha256": {p: sha(SNAPSHOT / p) for p in SOURCE_HASHES},
                    "runner_sha256": sha(Path(__file__)), "chromium": executable,
                    "control_sha256": sha(Path(author_control.__file__)) if paired else None,
                    "enabled": {"undo_guard": True, "select_llm": True, "safe": False,
                                "planner": False, "escalate": False, "verify_done": False, "macros": False},
                    "warmup_counts": dict(counts)}
        (output / "manifest.json").write_text(json.dumps(manifest, indent=2), encoding="utf-8")
        for original_task, arm, repeat in schedule:
            task.update(name=original_task[0], arm=arm, repeat=repeat, initial=None,
                        tokens={"qwen_input_tokens": 0, "qwen_output_tokens": 0})
            last_page.clear()
            emit("attempt_start")
            if paired:
                with measured("context_setup"):
                    context["id"] = raw_cdp("Target.createBrowserContext", disposeOnDetach=True)["browserContextId"]
            before = dict(counts)
            attempt_started = time.perf_counter()
            task["deadline"] = attempt_started + 180 if paired else float("inf")
            exhausted = False
            try:
                ok, steps, status, wall, final = suite.run(*original_task)
            except BudgetExhausted:
                ok, steps, status = False, None, "error:BudgetExhausted"
                wall, final = time.perf_counter() - attempt_started, last_page.get("url", "")
                exhausted = True
            passed = author_control.accepted(ok, status, task["name"], last_page) if paired else ok
            row = {"task": task["name"], "arm": arm, "repeat": repeat, "pass": passed, "author_pass": ok,
                   "steps": steps, "status": status, "wall_seconds": wall,
                   "attempt_wall_seconds": time.perf_counter() - attempt_started, "initial_observation_sha256": task["initial"],
                   "final_url": final, "counts": {k: None if v is None or before[k] is None else v - before[k]
                                                   for k, v in counts.items()}}
            row["counts"].update(task["tokens"])
            rows.append(row)
            if task["name"] == "internet-checkbox":
                row["independent_first_checkbox_checked"] = first_checkbox_checked(last_page)
            with (output / "attempts.jsonl").open("a", encoding="utf-8") as handle:
                handle.write(json.dumps(row) + "\n")
            print(json.dumps(row), flush=True)
            if paired:
                with measured("context_cleanup"):
                    raw_cdp("Target.disposeBrowserContext", browserContextId=context["id"])
                    context["id"] = None
            if exhausted:
                break
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
        report = {"completed": len(rows), "unstarted": planned - len(rows),
                  "passed": sum(row["pass"] for row in rows), "counts_including_warmup": counts,
                  "session_wall_seconds": time.perf_counter() - started, "rows": rows}
        if paired:
            report["comparison"] = author_control.compare(rows)
        (output / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
        events.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--chromium", type=Path)
    parser.add_argument("--paired", action="store_true", help="Run the local Qwen control and Laya arm with the shared null-guard fix")
    parser.add_argument("--repeats", type=int, choices=(1, 2, 3), default=1)
    parser.add_argument("--controlled", action="store_true", help="Unify common Qwen sampling parameters")
    parser.add_argument("--proxy", help="Explicit local HTTP proxy for the browser and Qwen; local services bypass it")
    parser.add_argument("--qwen-http-limit", type=int, help="Session HTTP cap, at most 360, including retries")
    args = parser.parse_args()
    run(args.output.resolve(), args.chromium, args.paired, args.repeats, args.controlled, args.proxy, args.qwen_http_limit)
