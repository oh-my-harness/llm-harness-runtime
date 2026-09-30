"""Deployment checks only; no Agent benchmark or accuracy claim."""

import argparse
from datetime import datetime, timezone
import importlib.metadata
import json
import os
from pathlib import Path
import platform
import time
import urllib.error
import urllib.request


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def qwen_smoke(config, send=None):
    key = os.environ.get(config["api_key_env"])
    if not key:
        return {"status": "blocked", "reason": "missing_api_key", "actual_calls": 0}
    # Never accept a config that could forward the user's credential elsewhere.
    if config["endpoint"] != "https://api.siliconflow.cn/v1/chat/completions":
        raise ValueError("Unexpected provider endpoint")
    body = {
        "model": config["model"],
        "messages": [{"role": "user", "content": "Reply with exactly OK."}],
        "temperature": config["temperature"],
        "max_tokens": config["max_tokens"],
        "stream": False,
    }
    request = urllib.request.Request(
        config["endpoint"],
        data=json.dumps(body).encode("utf-8"),
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    send = send or urllib.request.build_opener(NoRedirect).open
    result = {
        "actual_calls": 1, "retries": 0, "usage": None, "model": config["model"]
    }
    start = time.perf_counter_ns()
    try:
        with send(request, timeout=config["timeout_seconds"]) as response:
            payload = json.load(response)
        content = payload["choices"][0]["message"]["content"]
        result.update(
            status="ok" if isinstance(content, str) and content.strip() else "invalid_response",
            returned_model=payload.get("model"),
            usage={
                name: payload.get("usage", {}).get(name)
                for name in ("prompt_tokens", "completion_tokens", "total_tokens")
            },
        )
    except Exception as error:
        # Exception messages and provider bodies may contain credentials or request data.
        result.update(status="error", error_type=type(error).__name__)
        if isinstance(error, urllib.error.HTTPError):
            result["http_status"] = error.code
            error.close()
    finally:
        result["elapsed_ms"] = (time.perf_counter_ns() - start) / 1_000_000
    return result


def laya_smoke(config):
    import laya
    import torch

    if not torch.cuda.is_available():
        return {"status": "blocked", "reason": "cuda_unavailable", "actual_calls": 0}
    # Deployment validation uses one fixed checkpoint, never the automatic Router.
    os.environ["LAYA_CUDA_AMP"] = "bf16"
    start = time.perf_counter_ns()
    agent = laya.load(config["model"], revision=config["revision"], device=config["device"])
    torch.cuda.synchronize()
    result = {
        "status": "ok",
        "status_meaning": "interface_and_device_only_not_accuracy",
        "model": config["model"],
        "revision": agent.revision,
        "load_ms": (time.perf_counter_ns() - start) / 1_000_000,
        "device": str(agent.device),
        "gpu": torch.cuda.get_device_name(),
        "torch": torch.__version__,
        "cuda": torch.version.cuda,
        "autocast_dtype": str(agent.dtype),
        "amp_enabled": agent.amp_enabled,
        "packages": {
            name: importlib.metadata.version(name)
            for name in ("laya", "torch", "transformers", "huggingface-hub", "safetensors", "numpy")
        },
        "events": [],
    }
    if agent.revision != config["revision"] or agent.device.type != "cuda":
        result.update(status="error", reason="revision_or_device_mismatch", actual_calls=0)
        return result
    direct = json.loads(
        importlib.metadata.distribution("laya").read_text("direct_url.json") or "{}"
    )
    result["code_revision"] = direct.get("vcs_info", {}).get("commit_id")
    if result["code_revision"] != config["code_revision"]:
        result.update(status="error", reason="code_revision_mismatch", actual_calls=0)
        return result

    state = (
        "The test tool returned exit code 1. Its output says an assertion failed. "
        "No repair has been generated."
    )
    questions = {
        "next_action": {
            "type": "choice",
            "instructions": "Choose the next action from the current state.",
            "criteria": {
                "request_repair": "Ask the generator to repair the failed code",
                "finish": "Declare the task complete",
            },
        }
    }
    result["smoke_input"] = {"state": state, "questions": questions}
    forwards = []

    def before_forward(module, args):
        forwards.append(str(next(module.parameters()).device))

    handle = agent.model.register_forward_pre_hook(before_forward)
    total = config["warmup_calls"] + config["measured_calls"]
    try:
        for index in range(total):
            torch.cuda.synchronize()
            before = len(forwards)
            start = time.perf_counter_ns()
            event = {
                "phase": "warmup" if index < config["warmup_calls"] else "smoke",
                "index": index,
            }
            try:
                response = agent.predict(
                    state, questions,
                    max_len=config["max_len"], head_max_len=config["head_max_len"],
                )
                choice = response["answers"]["next_action"]["choice"]
                valid = choice in questions["next_action"]["criteria"]
                event.update(
                    status="ok" if valid else "invalid_choice",
                    choice=choice, usage=response.get("usage"),
                )
            except Exception as error:
                event.update(status="error", error_type=type(error).__name__)
            finally:
                torch.cuda.synchronize()
                event["elapsed_ms"] = (time.perf_counter_ns() - start) / 1_000_000
                event["forward_devices"] = forwards[before:]
                event["forward_attempts"] = len(forwards) - before
                result["events"].append(event)
            if (
                event["status"] != "ok"
                or event["forward_attempts"] != 1
                or any(not d.startswith("cuda") for d in event["forward_devices"])
            ):
                result["status"] = "error"
    finally:
        handle.remove()
    result["actual_calls"] = len(result["events"])
    result["forward_attempts"] = len(forwards)
    result["cpu_fallback_count"] = agent.cpu_fallback_count
    if result["cpu_fallback_count"]:
        result["status"] = "error"
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--component", choices=("qwen", "laya"), required=True)
    parser.add_argument(
        "--config", type=Path,
        default=Path(__file__).resolve().parents[2] / "configs/research/laya-qwen-preflight.json",
    )
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    config = json.loads(args.config.read_text(encoding="utf-8"))
    if config["scope"] != "deployment_smoke_only":
        parser.error("This script is not an Agent benchmark")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    # Refuse to overwrite earlier checks, including failed ones.
    with args.output.open("x", encoding="utf-8") as stream:
        start = time.perf_counter_ns()
        try:
            check = qwen_smoke if args.component == "qwen" else laya_smoke
            result = check(config[args.component])
        except Exception as error:
            result = {"status": "error", "error_type": type(error).__name__}
        report = {
            "scope": config["scope"],
            "completed_at_utc": datetime.now(timezone.utc).isoformat(),
            "component": args.component,
            "python": platform.python_version(),
            "platform": platform.platform(),
            "result": result,
            "total_ms": (time.perf_counter_ns() - start) / 1_000_000,
        }
        stream.write(json.dumps(report, ensure_ascii=True, indent=2) + "\n")
    print(json.dumps(report, ensure_ascii=True))
    return 0 if result["status"] == "ok" else 1


if __name__ == "__main__":
    raise SystemExit(main())
