"""Real Qwen HTTP and pinned Laya adapters; no rules, mock arm, or fallback."""

import importlib.metadata
import json
import os
import time
import urllib.request

from preflight_laya_qwen import NoRedirect


class Qwen:
    def __init__(self, config):
        if config["endpoint"] != "https://api.siliconflow.cn/v1/chat/completions":
            raise ValueError("Unexpected credential destination")
        self.config = config
        self.key = os.environ[config["api_key_env"]]
        self.opener = urllib.request.build_opener(NoRedirect)

    def call(self, system, state, event, remaining):
        body = {
            "model": self.config["model"], "temperature": self.config["temperature"],
            "max_tokens": self.config["max_tokens"], "stream": False,
            "response_format": {"type": "json_object"},
            "messages": [{"role": "system", "content": system},
                         {"role": "user", "content": state}],
        }
        request = urllib.request.Request(
            self.config["endpoint"], data=json.dumps(body).encode("utf-8"),
            headers={"Authorization": "Bearer " + self.key, "Content-Type": "application/json"},
        )
        event.update(actual_calls=1, usage=None, retry_index=0)
        # urllib has no SDK automatic retry and redirects are explicitly refused.
        with self.opener.open(request, timeout=min(remaining, self.config["timeout_seconds"])) as response:
            payload = json.load(response)
        event["usage"] = payload.get("usage")
        event["returned_model"] = payload.get("model")
        event["finish_reason"] = payload["choices"][0].get("finish_reason")
        content = payload["choices"][0]["message"]["content"]
        if not isinstance(content, str) or not content.strip():
            raise ValueError("Empty model response")
        return content


class Laya:
    def __init__(self, config):
        import laya
        import torch

        if config["selection_status"] != "approved":
            raise ValueError("The proposed checkpoint is awaiting user confirmation")
        if not torch.cuda.is_available():
            raise RuntimeError("CUDA unavailable; CPU fallback is not allowed")
        os.environ["LAYA_CUDA_AMP"] = "bf16"
        self.torch, self.config = torch, config
        start = time.perf_counter_ns()
        self.agent = laya.load(config["model"], revision=config["revision"], device="cuda")
        torch.cuda.synchronize()
        direct = json.loads(importlib.metadata.distribution("laya").read_text("direct_url.json"))
        if direct["vcs_info"]["commit_id"] != config["code_revision"]:
            raise ValueError("Laya code revision mismatch")
        if self.agent.revision != config["revision"] or self.agent.device.type != "cuda":
            raise ValueError("Laya weight revision or device mismatch")
        self.forwards = []
        self.hook = self.agent.model.register_forward_pre_hook(
            lambda model, args: self.forwards.append(str(next(model.parameters()).device))
        )
        self.setup = {
            "load_ms": (time.perf_counter_ns() - start) / 1e6,
            "model": config["model"], "revision": self.agent.revision,
            "gpu": torch.cuda.get_device_name(), "torch": torch.__version__,
            "cuda": torch.version.cuda, "autocast_dtype": str(self.agent.dtype),
            "warmup": [],
        }
        for _ in range(config["warmup_calls"]):
            event = {}
            start = time.perf_counter_ns()
            self.choose("Deployment warmup only.", [{"id": "a", "description": "Warm up"},
                                                   {"id": "b", "description": "Wait"}], event)
            event["elapsed_ms"] = (time.perf_counter_ns() - start) / 1e6
            self.setup["warmup"].append(event)

    def choose(self, state, candidates, event):
        event.update(actual_calls=0, forward_attempts=0, retry_index=0, usage=None)
        tokens = self.agent.tok(state, add_special_tokens=False)["input_ids"]
        event["state_tokens"] = len(tokens)
        event["state_truncated"] = False
        if len(tokens) > self.config["max_len"] - self.config["head_max_len"] - 16:
            raise ValueError("Laya state exceeds frozen budget; no silent truncation")
        mapping = {"C" + str(index): candidate for index, candidate in enumerate(candidates)}
        # Candidate descriptions also exist in the full state. Short head labels remain distinct.
        questions = {"action": {
            "type": "choice",
            "instructions": "Which next action best advances the user task given the observations?",
            "criteria": {key: candidate["description"] for key, candidate in mapping.items()},
        }}
        before = len(self.forwards)
        self.torch.cuda.synchronize()
        event["actual_calls"] = 1
        try:
            response = self.agent.predict(
                state, questions, max_len=self.config["max_len"],
                head_max_len=self.config["head_max_len"],
            )
            event["usage"] = response.get("usage")
            if response.get("usage", {}).get("options"):
                raise ValueError("Laya candidate descriptions collapsed under the head budget")
            event["answer"] = response["answers"]["action"]
            return mapping[event["answer"]["choice"]]["id"]
        finally:
            self.torch.cuda.synchronize()
            event["forward_devices"] = self.forwards[before:]
            event["forward_attempts"] = len(self.forwards) - before
            event["cpu_fallback_count"] = self.agent.cpu_fallback_count
            if self.agent.cpu_fallback_count or any(not d.startswith("cuda") for d in self.forwards[before:]):
                raise RuntimeError("Laya silently changed execution device")
