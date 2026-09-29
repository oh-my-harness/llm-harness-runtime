"""Real Qwen HTTP and pinned Laya adapters; no rules, mock arm, or fallback."""

import importlib.metadata
import json
import os
import time
import urllib.request

from preflight_laya_qwen import NoRedirect


class InputBudgetError(ValueError):
    """The pinned encoder would lose decision information before inference."""


def audit_laya_input(agent, state, questions, max_len, head_max_len):
    # Use the pinned SDK's serializer and sequence builder, not an estimated token count.
    from laya.common import build_sequence, encode_text, render_options, serialize_state

    tok = agent.tok
    ids = encode_text(tok, serialize_state(state).replace(tok.mask_token, " "),
                      add_special_tokens=False)["input_ids"]
    audit = {}
    for name, question in questions.items():
        agent._check_question(name, question)
        internal = agent._to_internal(question)
        empty, markers, stats = build_sequence(
            tok, "", internal, max_len, head_max_len, state_ids=[], return_stats=True,
        )
        # With ample head space the SDK keeps the whole instruction, but still caps each option at 48.
        full, _, _ = build_sequence(tok, "", internal, 1000000, 1000000,
                                     state_ids=[], return_stats=True)
        options = render_options(internal)
        option_tokens = [len(encode_text(tok, " " + option.replace(tok.mask_token, " "),
                                        add_special_tokens=False)["input_ids"]) for option in options]
        room = max(0, max_len - len(empty))
        audit[name] = {
            "state_tokens": len(ids), "state_room": room,
            "state_tokens_dropped": max(0, len(ids) - room),
            "head_tokens_dropped": max(0, len(full) - len(empty)),
            "option_tokens_dropped_at_48": sum(max(0, n - 48) for n in option_tokens),
            "markers": len(markers), **stats,
        }
    return audit


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
    def __init__(self, config, *, input_verification_only=False):
        import laya
        import torch

        if config["selection_status"] != "approved" and not input_verification_only:
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
            "scope": "input_verification_only" if input_verification_only else "approved_run",
            "load_ms": (time.perf_counter_ns() - start) / 1e6,
            "model": config["model"], "revision": self.agent.revision,
            "gpu": torch.cuda.get_device_name(), "torch": torch.__version__,
            "cuda": torch.version.cuda, "autocast_dtype": str(self.agent.dtype),
            "warmup": [],
        }
        for _ in range(config["warmup_calls"]):
            event = {}
            start = time.perf_counter_ns()
            self.predict("Deployment warmup only.", {"warmup": {
                "type": "choice", "instructions": "Choose a warmup action.",
                "criteria": {"a": "Warm up", "b": "Wait"},
            }}, event)
            event["elapsed_ms"] = (time.perf_counter_ns() - start) / 1e6
            self.setup["warmup"].append(event)

    def predict(self, state, questions, event):
        event.update(actual_calls=0, forward_attempts=0, retry_index=0, usage=None)
        event["decision_questions"] = len(questions)
        event["input_audit"] = audit_laya_input(
            self.agent, state, questions, self.config["max_len"], self.config["head_max_len"],
        )
        if any(a["state_tokens_dropped"] or a["head_tokens_dropped"]
               or a["option_tokens_dropped_at_48"] or a["markers"] != a["options"]
               or a["options_distinct"] != a["options"] for a in event["input_audit"].values()):
            raise InputBudgetError("Pinned Laya budgets lose state, instructions, or candidate content")
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
            event["answers"] = response["answers"]
            return response["answers"]
        finally:
            self.torch.cuda.synchronize()
            event["forward_devices"] = self.forwards[before:]
            event["forward_attempts"] = len(self.forwards) - before
            event["cpu_fallback_count"] = self.agent.cpu_fallback_count
            if self.agent.cpu_fallback_count or any(not d.startswith("cuda") for d in self.forwards[before:]):
                raise RuntimeError("Laya silently changed execution device")
