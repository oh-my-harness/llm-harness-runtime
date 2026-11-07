"""Bounded offline Kev deployment checks, with the existing browser wrapper unchanged."""

import argparse
from copy import deepcopy
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from importlib.metadata import distributions
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import sys
import threading
import time
import traceback
import urllib.request
from urllib.parse import urlparse

ROOT = Path(__file__).resolve().parents[3]
SOURCE = ROOT.parent / "benchmark/sources/kev"
CODE_REVISION = "0fe8fc97c2bcc247fa3efb6e5c32af4e99770e91"
REVISION = "c4bfa11b0dc07691884f2d97f1c4c4c05c92e416"
BASE_REVISION = "906bfd4b4dc7f14ee4320094d8b41684abff8539"
WRAPPER = ROOT.parent / "benchmark/models/huggingface/hub/models--cklxx--laya-browser/snapshots/645cf366a2ae35f1086e8c20eff48f909bb49206/code/apps/laya_browser.py"
WRAPPER_SHA = "3dfaba4030a1c96afcd8f5a865a66518e5edb07c6b332e0668f1519a624e6b03"
ADAPTER_HASHES = {
    "adapter_model.safetensors": "3fa10d8646f567e884ea47841b2f3d5f2d135b81a4447a8ea79a6fc94e9c6ae1",
    "head.pt": "dad87f7e60c8b3f1eec8bee83858a5429c6ab06dc96385a935bc88e6b468a45d",
    "tokenizer.json": "aeb13307a71acd8fe81861d94ad54ab689df773318809eed3cbe794b4492dae4",
}
BASE_HASHES = {
    "model-00001-of-00003.safetensors": "4c807e2503d68ae373d508689d00a41f4b33f33c2536da97ab81a20caddc1241",
    "model-00002-of-00003.safetensors": "f4707585548b2fc75a6b1d732e8465c62040a8699903c32850781beeb9b27826",
    "model-00003-of-00003.safetensors": "c7b1aa8fb672de2e00423c99876926022e50b18d4f0d140670788510a27f9965",
}


def file_sha(path):
    with Path(path).open("rb") as handle:
        return hashlib.file_digest(handle, "sha256").hexdigest()


def digest(value):
    return hashlib.sha256(json.dumps(value, ensure_ascii=False, sort_keys=True).encode()).hexdigest()


def save(path, value):
    Path(path).write_text(json.dumps(value, ensure_ascii=False, indent=2), encoding="utf-8")


def validate(questions, result):
    answers = result.get("answers", {})
    if set(questions) != set(answers):
        raise ValueError("Question IDs changed")
    for qid, q in questions.items():
        a = answers[qid]
        p = a.get("probabilities", {})
        if q["type"] != "choice" or set(p) != set(q["criteria"]) or a.get("choice") not in p:
            raise ValueError("Candidate IDs changed")
        if any(type(v) not in (int, float) or not math.isfinite(v) or not 0 <= v <= 1 for v in p.values()):
            raise ValueError("Invalid probabilities")
        if abs(sum(p.values()) - 1) > .02:
            raise ValueError("Probabilities are not normalized")
    return result


def differences(left, right):
    if left.keys() != right.keys():
        raise ValueError("Question IDs differ")
    flips, delta = [], 0.
    for qid, a in left.items():
        b = right[qid]
        if a["probabilities"].keys() != b["probabilities"].keys():
            raise ValueError("Candidate IDs differ")
        if a["choice"] != b["choice"]:
            flips.append(qid)
        delta = max(delta, *(abs(p - b["probabilities"][k]) for k, p in a["probabilities"].items()))
    return {"choice_flips": flips, "max_probability_delta": delta}


def post(url, body):
    request = urllib.request.Request(url, json.dumps(body).encode(), {"Content-Type": "application/json"})
    start = time.perf_counter()
    with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(request, timeout=90) as response:
        result = json.load(response)
    return result, time.perf_counter() - start


def verify_snapshot(path, expected=None):
    hashes = {}
    for item in sorted(Path(path).iterdir()):
        if not item.is_file():
            continue
        hashes[item.name] = file_sha(item)
        blob = item.resolve().name
        # Xet cache names are NOT file SHA-256 values. Compare against the pinned Hub LFS metadata.
        if len(blob) == 40:
            content = item.read_bytes()
            actual = hashlib.sha1(f"blob {len(content)}\0".encode() + content).hexdigest()
            if actual != blob:
                raise ValueError(f"Hub Git blob mismatch: {item.name}")
    if any(hashes.get(name) != value for name, value in (expected or {}).items()):
        raise ValueError("Snapshot does not match the pinned Hub LFS metadata")
    return hashes


class KevPredictAdapter:
    """Keep LayaBrowser's preprocessing/chunks; delegate scoring to the author's Server."""

    def __init__(self, server):
        self.server, self.calls, self.forwards = server, [], 0
        self.hook = server.model.lm.register_forward_pre_hook(self._forward)

    def _forward(self, module, args):
        if next(module.parameters()).device.type != "cuda":
            raise RuntimeError("Unexpected CPU forward")
        self.forwards += 1

    def predict(self, state, questions):
        from kev.api import SystemOneRequest, to_record
        from kev.model import SERVE_MAX_BRANCH, SERVE_MAX_STATE
        req = SystemOneRequest(state=state, questions=questions)
        rec, _ = to_record(req)
        enc = self.server.model.encode(self.server.tok, rec, max_state=SERVE_MAX_STATE,
                                       max_branch=SERVE_MAX_BRANCH, strict=True)
        if enc["state_truncated"] or len(enc["opt_idx"]) != len(questions):
            raise ValueError("Input was truncated")
        if [len(x) for x in enc["opt_idx"]] != [len(q["criteria"]) for q in questions.values()]:
            raise ValueError("Encoded candidates changed")
        before, start = self.forwards, time.perf_counter()
        result = validate(questions, self.server.answer(req))
        self.calls.append({"input_sha256": digest({"state": state, "questions": questions}),
                           "ordered_candidates": {qid: list(q["criteria"]) for qid, q in questions.items()},
                           "state_tokens": enc["seg"].count(0), "packed_tokens": len(enc["ids"]),
                           "candidate_counts": [len(x) for x in enc["opt_idx"]],
                           "forward_count": self.forwards - before,
                           "native_usage": result["usage"], "generated_tokens": 0,
                           "server_model_ms": result["latency_ms"],
                           "adapter_seconds": time.perf_counter() - start})
        return result


def run(corpus, output, port):
    output.mkdir(parents=True, exist_ok=False)
    report = {"complete": False, "environment_ready": False, "formal_experiment_ready": False,
              "remote_llm_requests": 0, "live_browser_tasks": 0, "cases": [],
              "code_revision": CODE_REVISION, "model_revision": REVISION, "base_revision": BASE_REVISION,
              "corpus_sha256": file_sha(corpus), "script_sha256": file_sha(__file__),
              "python": sys.version, "platform": platform.platform()}
    server = http = thread = None
    try:
        if subprocess.check_output(["git", "-C", str(SOURCE), "rev-parse", "HEAD"], text=True).strip() != CODE_REVISION:
            raise ValueError("Kev source revision changed")
        # A Windows checkout has CRLF; use its checkout normalization when Git runs from WSL.
        if subprocess.check_output(["git", "-c", "core.autocrlf=true", "-C", str(SOURCE), "diff", "HEAD", "--", "kev", "uv.lock", "pyproject.toml"]):
            raise ValueError("Kev inference source modified")
        if file_sha(WRAPPER) != WRAPPER_SHA:
            raise ValueError("Browser wrapper changed")
        if os.environ.get("KEV_DATE_FACTS", "0") != "0":
            raise ValueError("Date rules must stay disabled")
        os.environ.update(HF_HUB_OFFLINE="1", TRANSFORMERS_OFFLINE="1", TOKENIZERS_PARALLELISM="false")
        import torch
        from kev.api import SystemOneRequest
        from kev.checkpoint import Checkpoint, LoadOptions, fused_available
        from kev.serve import Server
        if not torch.cuda.is_available():
            raise RuntimeError("CUDA unavailable; no CPU fallback")
        sys.path.insert(0, str(WRAPPER.parent))
        from laya_browser import LayaBrowser
        data = json.loads(corpus.read_text(encoding="utf-8"))
        for case in data["cases"]:
            if digest(case["body"]) != case["sha256"]:
                raise ValueError("Frozen request changed")
        ck = Checkpoint(f"jaredpalmer/kev-4b@{REVISION}")
        if ck.meta.base != "Qwen/Qwen3-4B-Base" or ck.meta.base_revision != BASE_REVISION:
            raise ValueError("Wrong base model")
        from huggingface_hub import snapshot_download
        base = snapshot_download(ck.meta.base, revision=BASE_REVISION, local_files_only=True,
                                 allow_patterns=["*.json", "*.safetensors", "*.txt", "*.jinja"])
        report["weight_files_sha256"] = {"adapter": verify_snapshot(ck.path, ADAPTER_HASHES),
                                         "base": verify_snapshot(base, BASE_HASHES)}
        report["packages"] = {d.metadata["Name"]: d.version for d in distributions()}
        report["gpu"] = {"name": torch.cuda.get_device_name(), "capability": torch.cuda.get_device_capability(),
                         "cuda": torch.version.cuda, "free_total_bytes": torch.cuda.mem_get_info()}
        print("Checksums verified; loading the pinned checkpoint", flush=True)
        start = time.perf_counter()
        # Match kev.serve's CUDA defaults; the attention-only base does not enter hybrid-only kernels/graphs.
        opts = LoadOptions(dtype=torch.bfloat16, cuda_graphs=True, fused=fused_available(), backend="auto")
        tok, model = ck.load("cuda", opts)
        torch.cuda.synchronize()
        report["load_seconds"] = time.perf_counter() - start
        if model.hybrid or model.graphs is not None or model.dtype != "bfloat16":
            raise ValueError("Unexpected serving path")
        report["model"] = {"base": ck.meta.base, "parameters": sum(p.numel() for p in model.parameters()),
                           "dtype": model.dtype, "attention": model.lm.config._attn_implementation,
                           "hybrid": model.hybrid, "cuda_graphs": False, "date_facts": False,
                           "temperature": model.head.temperature, "backend": model.backend}
        print(json.dumps({"loaded": report["model"], "seconds": report["load_seconds"]}), flush=True)
        server = Server(ck, tok, model, "cuda", release_date="unknown-offline")
        adapter = KevPredictAdapter(server)
        wrapper = LayaBrowser(adapter)

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                if self.path != "/v1/systemone":
                    self.send_error(404)
                    return
                try:
                    body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    result = wrapper.systemone(body)
                    result["model"] = f"kev-4b@{REVISION}"
                    encoded = json.dumps(result).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(encoded)))
                    self.end_headers()
                    self.wfile.write(encoded)
                except Exception as error:
                    self.send_error(500, type(error).__name__)

        http = ThreadingHTTPServer(("127.0.0.1", port), Handler)
        thread = threading.Thread(target=http.serve_forever, daemon=True)
        thread.start()
        url = f"http://127.0.0.1:{port}/v1/systemone"
        for case in data["cases"]:
            server.prefix_cache.clear()
            row = {"name": case["name"], "request_sha256": case["sha256"], "calls": []}
            for repeat in range(4):
                before, forward = len(adapter.calls), adapter.forwards
                result, seconds = post(url, case["body"])
                validate(case["body"]["questions"], result)
                row["calls"].append({"phase": "first" if repeat == 0 else "warm", "wall_seconds": seconds,
                                     "answers": result["answers"], "predicts": adapter.calls[before:],
                                     "forwards": adapter.forwards - forward})
            row["stability"] = [differences(row["calls"][0]["answers"], x["answers"]) for x in row["calls"][1:]]
            row["warm_median_seconds"] = statistics.median(x["wall_seconds"] for x in row["calls"][1:])
            report["cases"].append(row)
            save(output / "progress.json", report)
            print(json.dumps({"case": case["name"], "warm_median_seconds": row["warm_median_seconds"],
                              "flips": [x["choice_flips"] for x in row["stability"]]}), flush=True)
        # Profile separately from wall-clock samples; the model worker runs on another thread.
        with server.lock, torch.profiler.profile(activities=[torch.profiler.ProfilerActivity.CPU,
                                                            torch.profiler.ProfilerActivity.CUDA]) as prof:
            from kev.api import to_record
            rec, _ = to_record(SystemOneRequest.model_validate(data["cases"][0]["body"]))
            enc = model.encode(tok, rec, max_state=65536, max_branch=73728, strict=True)
            with torch.inference_mode():
                model.probs(enc)
            torch.cuda.synchronize()
        report["cuda_kernel_names"] = sorted({e.name for e in prof.events() if "CUDA" in str(e.device_type)})
        report["cache"] = {k: getattr(server.prefix_cache, k) for k in ("hits", "misses", "oom_retries", "size")}
        report["memory"] = {"peak_allocated_bytes": torch.cuda.max_memory_allocated(),
                            "peak_reserved_bytes": torch.cuda.max_memory_reserved()}
        # Windows probes only after timed samples, so concurrent callers cannot change batch sizes/timings.
        save(output / "ready.json", {"url": url, "body": data["cases"][0]["body"]})
        print("Ready for Windows loopback probe", flush=True)
        deadline = time.monotonic() + 120
        while not (output / "windows-probe.json").exists() and time.monotonic() < deadline:
            time.sleep(.5)
        if (output / "windows-probe.json").exists():
            report["windows_probe"] = json.loads((output / "windows-probe.json").read_text(encoding="utf-8"))
            report["windows_parity"] = differences(report["cases"][0]["calls"][-1]["answers"],
                                                   report["windows_probe"]["response"]["answers"])
        report["complete"] = True
        report["environment_ready"] = (bool(report["cuda_kernel_names"]) and report["cache"]["oom_retries"] == 0
            and not any(x["choice_flips"] for row in report["cases"] for x in row["stability"])
            and "windows_parity" in report and not report["windows_parity"]["choice_flips"])
    except Exception as error:
        report["error"] = {"type": type(error).__name__, "message": str(error), "traceback": traceback.format_exc()}
        print(traceback.format_exc(), flush=True)
    finally:
        if http:
            http.shutdown()
            http.server_close()
        if thread:
            thread.join()
        if server:
            server.close()
        save(output / "report.json", report)
    return report["environment_ready"]


def probe(output):
    data = json.loads((output / "ready.json").read_text(encoding="utf-8"))
    parsed = urlparse(data["url"])
    if parsed.scheme != "http" or parsed.hostname != "127.0.0.1" or parsed.path != "/v1/systemone":
        raise ValueError("Only loopback probes allowed")
    sys.path.insert(0, str(ROOT.parent / "benchmark/sources/jev-ultrafast"))
    from jev_ultrafast import model
    import httpx
    model.CLIENT.close()
    model.CLIENT = httpx.Client(http2=True, timeout=25, trust_env=False)
    start = time.perf_counter()
    try:
        response = model.post_json(data["url"], "local", data["body"])
        seconds = time.perf_counter() - start
        for qid, q in data["body"]["questions"].items():
            model.validate_choice(response["answers"][qid], q["criteria"])
    finally:
        model.CLIENT.close()
    validate(data["body"]["questions"], response)
    save(output / "windows-probe.json", {"platform": platform.platform(), "wall_seconds": seconds,
                                        "client": "jev_ultrafast.model.post_json + validate_choice",
                                        "client_sha256": file_sha(model.__file__), "response": response})
    print(json.dumps({"loopback_ok": True, "seconds": seconds}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=("run", "probe"))
    parser.add_argument("--corpus", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--port", type=int, default=18792)
    args = parser.parse_args()
    if args.mode == "probe":
        probe(args.output)
    else:
        raise SystemExit(0 if run(args.corpus, args.output, args.port) else 1)
