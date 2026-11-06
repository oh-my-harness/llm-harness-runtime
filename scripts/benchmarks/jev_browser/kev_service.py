"""Owned WSL Kev service and a durable, request-scoped inference ledger."""

import argparse
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time
import urllib.request
import uuid

import kev_preflight as preflight

MODEL = f"kev-4b@{preflight.REVISION}"
COUNTERS = ("kev_rpc", "kev_predict", "kev_forward")


class KevUnavailable(BaseException):
    """Abort the matrix: an unaccounted local worker must not overlap the next task."""


def wsl_path(path):
    path = Path(path).resolve()
    if len(path.drive) != 2 or path.drive[1] != ":":
        raise ValueError("Expected a local Windows drive path")
    return "/mnt/" + path.drive[0].lower() + path.as_posix()[2:]


def request(url, body=None, token=None, timeout=25):
    headers = {"Content-Type": "application/json"}
    if token:
        headers["X-Kev-Control"] = token
    req = urllib.request.Request(url, None if body is None else json.dumps(body).encode(), headers)
    with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(req, timeout=timeout) as response:
        return json.load(response)


class Ledger:
    def __init__(self, path):
        self.handle = path.open("x", encoding="utf-8")
        self.lock = threading.Lock()
        self.scope = {"task": "setup", "arm": None, "repeat": None}
        self.sequence = 0

    def emit(self, component, **data):
        with self.lock:
            self.sequence += 1
            row = {"service_seq": self.sequence, "utc": time.time(), **self.scope,
                   "component": component, **data}
            self.handle.write(json.dumps(row) + "\n")
            self.handle.flush()

    @contextmanager
    def measured(self, component, **data):
        start = time.perf_counter()
        self.emit(component + "_start", **data)
        try:
            yield data
            data["status"] = "ok"
        except BaseException as error:
            data.update(status="error", error_type=type(error).__name__)
            raise
        finally:
            self.emit(component, elapsed_ms=(time.perf_counter() - start) * 1000, **data)


class MeteredAdapter(preflight.KevPredictAdapter):
    def __init__(self, server, ledger):
        self.ledger = ledger
        super().__init__(server)

    def _forward(self, module, args):
        device = str(next(module.parameters()).device)
        self.ledger.emit("kev_forward", device=device)
        super()._forward(module, args)

    def predict(self, state, questions):
        before = self.forwards
        with self.ledger.measured("kev_predict", questions=len(questions)) as record:
            try:
                result = super().predict(state, questions)
                record.update(self.calls[-1])
                return result
            finally:
                record["forward_count"] = self.forwards - before


def load_server():
    if subprocess.check_output(["git", "-C", str(preflight.SOURCE), "rev-parse", "HEAD"], text=True).strip() != preflight.CODE_REVISION:
        raise ValueError("Kev revision changed")
    if subprocess.check_output(["git", "-c", "core.autocrlf=true", "-C", str(preflight.SOURCE),
                                "diff", "HEAD", "--", "kev", "uv.lock", "pyproject.toml"]):
        raise ValueError("Kev source modified")
    if preflight.file_sha(preflight.WRAPPER) != preflight.WRAPPER_SHA:
        raise ValueError("Browser wrapper changed")
    if os.environ.get("KEV_DATE_FACTS", "0") != "0":
        raise ValueError("Date rules must stay disabled")
    import torch
    from huggingface_hub import snapshot_download
    from kev.checkpoint import Checkpoint, LoadOptions, fused_available
    from kev.serve import Server
    if not torch.cuda.is_available():
        raise RuntimeError("CUDA unavailable")
    ck = Checkpoint(f"jaredpalmer/kev-4b@{preflight.REVISION}")
    if ck.meta.base != "Qwen/Qwen3-4B-Base" or ck.meta.base_revision != preflight.BASE_REVISION:
        raise ValueError("Base model changed")
    base = snapshot_download(ck.meta.base, revision=preflight.BASE_REVISION, local_files_only=True,
                             allow_patterns=["*.json", "*.safetensors", "*.txt", "*.jinja"])
    hashes = {"adapter": preflight.verify_snapshot(ck.path, preflight.ADAPTER_HASHES),
              "base": preflight.verify_snapshot(base, preflight.BASE_HASHES)}
    start = time.perf_counter()
    tok, model = ck.load("cuda", LoadOptions(dtype=torch.bfloat16, cuda_graphs=True,
                                            fused=fused_available(), backend="auto"))
    torch.cuda.synchronize()
    if model.hybrid or model.graphs is not None or model.dtype != "bfloat16":
        raise ValueError("Serving path differs from preflight")
    metadata = {"model": MODEL, "code_revision": preflight.CODE_REVISION,
                "adapter_revision": preflight.REVISION, "base_revision": preflight.BASE_REVISION,
                "load_seconds": time.perf_counter() - start, "weight_files_sha256": hashes,
                "parameters": sum(p.numel() for p in model.parameters()), "dtype": model.dtype,
                "attention": model.lm.config._attn_implementation, "temperature": model.head.temperature,
                "device": "cuda:0", "gpu": torch.cuda.get_device_name(), "cuda": torch.version.cuda,
                "python": sys.version, "torch": torch.__version__, "date_facts": False,
                "cache_reset": "before every attempt, outside task clock",
                "service_sha256": preflight.file_sha(__file__),
                "preflight_sha256": preflight.file_sha(preflight.__file__)}
    server = Server(ck, tok, model, "cuda", release_date="unknown-offline")
    metadata["cache"] = {k: getattr(server.prefix_cache, k) for k in ("size", "min_tokens", "max_tokens")}
    if metadata["cache"] != {"size": 4, "min_tokens": 384, "max_tokens": 65536}:
        server.close()
        raise ValueError("Cache configuration differs from preflight")
    return server, metadata


def serve(output, port, token):
    # Record the Linux PID before loading so the owner can also stop failed startup.
    preflight.save(output / "kev-pid.json", {"pid": os.getpid(), "token": token})
    ledger = Ledger(output / "kev-events.jsonl")
    server = http = None
    stopping = threading.Event()
    try:
        with ledger.measured("kev_model_load"):
            server, metadata = load_server()
        sys.path.insert(0, str(preflight.WRAPPER.parent))
        from laya_browser import LayaBrowser
        wrapper = LayaBrowser(MeteredAdapter(server, ledger))
        inference_lock = threading.Lock()

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def reply(self, code, body):
                encoded = json.dumps(body).encode()
                self.send_response(code)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(encoded)))
                self.end_headers()
                self.wfile.write(encoded)

            def do_GET(self):
                self.reply(200, metadata) if self.path == "/" else self.send_error(404)

            def do_POST(self):
                if self.headers.get("X-Kev-Control") != token:
                    self.send_error(403)
                    return
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                if self.path == "/shutdown":
                    stopping.set()
                    self.reply(200, {"stopping": True})
                    threading.Thread(target=http.shutdown, daemon=True).start()
                    return
                with inference_lock:
                    if stopping.is_set():
                        self.send_error(500, "Service stopping")
                        return
                    if self.path == "/reset":
                        server.wait_idle()
                        with server.lock:
                            server.prefix_cache.clear()
                        self.reply(200, {"idle": True, "cached_states": len(server.prefix_cache.entries)})
                        return
                    if self.path != "/v1/systemone":
                        self.send_error(404)
                        return
                    ledger.scope = json.loads(self.headers["X-Kev-Scope"])
                    try:
                        with ledger.measured("kev_rpc", request_sha256=preflight.digest(body)) as record:
                            result = preflight.validate(body["questions"], wrapper.systemone(body))
                            result["model"] = MODEL
                            # Wrapper usage is placeholder zeros; actual usage is in the predict ledger.
                            result["usage"] = None
                            record["answers"] = result["answers"]
                        self.reply(200, result)
                    except Exception as error:
                        self.reply(500, {"error_type": type(error).__name__})

        http = ThreadingHTTPServer(("127.0.0.1", port), Handler)
        preflight.save(output / "kev-ready.json", metadata)
        http.serve_forever()
    finally:
        if http:
            http.server_close()
        if server:
            server.close()
            import torch
            preflight.save(output / "kev-closed.json", {"graceful": True,
                "peak_allocated_bytes": torch.cuda.max_memory_allocated(),
                "peak_reserved_bytes": torch.cuda.max_memory_reserved(),
                "cache": {k: getattr(server.prefix_cache, k) for k in ("hits", "misses", "oom_retries")}})
        ledger.handle.close()


class KevProcess:
    def __init__(self, output, port, emit, counts):
        self.output, self.url = output, f"http://127.0.0.1:{port}"
        self.port, self.emit, self.counts = port, emit, counts
        self.token, self.process, self.log = uuid.uuid4().hex, None, None
        self.offset, self.sequence = 0, 0

    def start(self):
        python = os.environ["KEV_WSL_PYTHON"]
        home = os.environ["KEV_HF_HOME"]
        self.distro = os.environ.get("KEV_WSL_DISTRO", "Ubuntu-24.04")
        if not python.startswith("/") or not home.startswith("/"):
            raise ValueError("Kev WSL paths must be absolute")
        self.log = (self.output / "kev-service.log").open("x", encoding="utf-8")
        # No shell interpolation; keys and unrelated Windows environment are not forwarded.
        self.process = subprocess.Popen(["wsl.exe", "-d", self.distro, "--exec", "env",
            f"HF_HOME={home}", "HF_HUB_OFFLINE=1", "TRANSFORMERS_OFFLINE=1",
            "TOKENIZERS_PARALLELISM=false", "KEV_DATE_FACTS=0", python, "-u", wsl_path(__file__),
            "--output", wsl_path(self.output), "--port", str(self.port), "--token", self.token],
            stdout=self.log, stderr=subprocess.STDOUT, creationflags=subprocess.CREATE_NO_WINDOW)
        deadline = time.monotonic() + 180
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError("Kev startup failed; inspect kev-service.log")
            try:
                metadata = request(self.url, timeout=1)
                if metadata.get("model") != MODEL:
                    raise ValueError("Unexpected local decision model")
                self.collect()
                return metadata
            except OSError:
                time.sleep(.25)
        raise RuntimeError("Kev startup timed out")

    def decorate(self, req, task):
        req.headers["X-Kev-Control"] = self.token
        req.headers["X-Kev-Scope"] = json.dumps({"task": task["name"], "arm": task["arm"], "repeat": task["repeat"]})

    def collect(self):
        path = self.output / "kev-events.jsonl"
        if not path.exists():
            return
        with path.open("rb") as handle:
            handle.seek(self.offset)
            for line in handle:
                if not line.endswith(b"\n"):
                    break
                row = json.loads(line)
                if row["service_seq"] != self.sequence + 1:
                    raise ValueError("Kev ledger sequence mismatch")
                self.sequence += 1
                self.offset += len(line)
                component = row.pop("component")
                counter = component.removesuffix("_start") if component.endswith("_start") else component
                if counter == "kev_forward" or (component.endswith("_start") and counter in COUNTERS):
                    self.counts[counter] += 1
                self.emit(component, **row)

    def reset(self):
        try:
            result = request(self.url + "/reset", {}, self.token)
        except Exception:
            raise KevUnavailable("Kev cache reset failed") from None
        if result != {"idle": True, "cached_states": 0}:
            raise KevUnavailable("Kev did not become idle")

    def close(self):
        if self.process is None:
            return
        try:
            if self.process.poll() is None:
                try:
                    request(self.url + "/shutdown", {}, self.token, timeout=2)
                except OSError:
                    pass
                try:
                    self.process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    pid_file = self.output / "kev-pid.json"
                    if not pid_file.exists():
                        raise RuntimeError("Cannot verify Kev Linux PID for cleanup")
                    pid = json.loads(pid_file.read_text(encoding="utf-8"))
                    if pid["token"] != self.token or type(pid["pid"]) is not int or pid["pid"] < 2:
                        raise RuntimeError("Unverified Kev PID")
                    subprocess.run(["wsl.exe", "-d", self.distro, "--exec", "kill", "-KILL", str(pid["pid"])],
                                   check=True, timeout=10, capture_output=True,
                                   creationflags=subprocess.CREATE_NO_WINDOW)
                    self.process.wait(timeout=10)
                    self.emit("kev_forced_shutdown")
            self.collect()
        finally:
            if self.log:
                self.log.close()


def smoke(output, port):
    """Exercise the real Windows client and owned service, without websites or an LLM."""
    output.mkdir(parents=True, exist_ok=False)
    events, counts = [], dict.fromkeys(COUNTERS, 0)
    proc = KevProcess(output, port, lambda component, **data: events.append({"component": component, **data}), counts)
    report = {"ready": False, "remote_llm_requests": 0, "live_browser_tasks": 0}
    sys.path.insert(0, str(preflight.ROOT.parent / "benchmark/sources/jev-ultrafast"))
    from jev_ultrafast import model
    import httpx
    model.CLIENT.close()
    model.CLIENT = httpx.Client(http2=True, timeout=25, trust_env=False)
    try:
        report["metadata"] = proc.start()
        sample = json.loads((preflight.WRAPPER.parent.parent / "sample_request.json").read_text(encoding="utf-8"))
        answers = []
        send = model.CLIENT.send
        def scoped_send(req, *args, **kwargs):
            proc.decorate(req, {"name": "smoke", "arm": "B", "repeat": 1})
            try:
                return send(req, *args, **kwargs)
            finally:
                proc.collect()
        model.CLIENT.send = scoped_send
        for _ in range(3):
            result = model.post_json(proc.url + "/v1/systemone", "local", sample)
            for qid, q in sample["questions"].items():
                model.validate_choice(result["answers"][qid], q["criteria"])
            answers.append(result["answers"])
        proc.reset()
        result = model.post_json(proc.url + "/v1/systemone", "local", sample)
        report["cache_reset_parity"] = preflight.differences(answers[-1], result["answers"])
        bad = model.CLIENT.post(proc.url + "/v1/systemone", json={"state": {}})
        if bad.status_code != 500 or counts["kev_rpc"] != 5:
            raise ValueError("Failure path did not record a failed RPC")
        report["ready"] = not report["cache_reset_parity"]["choice_flips"]
    finally:
        proc.close()
        model.CLIENT.close()
        report.update(counts=counts, events=events)
        report["graceful_shutdown"] = (output / "kev-closed.json").exists() and proc.process.returncode == 0
        report["ready"] = report["ready"] and report["graceful_shutdown"]
        preflight.save(output / "smoke.json", report)
    print(json.dumps({k: report[k] for k in ("ready", "counts", "graceful_shutdown")}))
    return report["ready"]


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--token")
    parser.add_argument("--smoke", action="store_true")
    args = parser.parse_args()
    if args.smoke:
        raise SystemExit(0 if smoke(args.output, args.port) else 1)
    if not args.token:
        parser.error("Service requires --token")
    serve(args.output, args.port, args.token)
