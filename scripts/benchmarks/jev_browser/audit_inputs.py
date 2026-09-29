"""Tokenization-only audit of captured jev-browser requests; never instantiate a model."""

import argparse
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import subprocess
import sys
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[3]
MODEL = "convaiinnovations/laya"
REVISION = "55cf4c4ebb4ebe31b2550e8bdf3bd21b99753851"
CODE_REVISION = "9d955671415fc19f069b9cc998928075c1f255ec"


def digest(value):
    return hashlib.sha256(json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()).hexdigest()


def compact_state_request(request):
    if not isinstance(request["state"], dict):
        raise ValueError("Expected the upstream structured state")
    state = json.dumps(request["state"], ensure_ascii=False, separators=(",", ":"))
    if json.loads(state) != request["state"]:
        raise ValueError("State compaction changed content")
    return {**request, "state": state}


def load_tokenizer_only():
    os.environ.update(HF_HUB_OFFLINE="1", TRANSFORMERS_OFFLINE="1")
    from laya.agent import Agent
    from transformers import AutoTokenizer

    installed = importlib.metadata.distribution("laya")
    direct = json.loads(installed.read_text("direct_url.json"))
    if direct["vcs_info"]["commit_id"] != CODE_REVISION:
        raise ValueError("Installed SDK revision mismatch")
    checkout = ROOT.parent / "benchmark/sources/laya"
    verified = {}
    for name in ("agent.py", "common.py", "__init__.py"):
        expected = subprocess.run(["git", "show", CODE_REVISION + ":laya/" + name],
                                  cwd=checkout, check=True, capture_output=True).stdout.replace(b"\r\n", b"\n")
        actual = Path(installed.locate_file("laya/" + name)).read_bytes().replace(b"\r\n", b"\n")
        if expected != actual:
            raise ValueError("Installed SDK source mismatch")
        verified[name] = hashlib.sha256(actual).hexdigest()
    snapshot = ROOT.parent / "benchmark/models/huggingface/hub/models--convaiinnovations--laya/snapshots" / REVISION
    config = json.loads((snapshot / "rl_agent_config.json").read_text())
    encoder = json.loads((snapshot / "encoder/config.json").read_text())
    tok = AutoTokenizer.from_pretrained(str(snapshot / "tokenizer"), local_files_only=True)
    # Only static validation and serializer methods are bound, not Agent.__init__/load/predict.
    view = SimpleNamespace(tok=tok, _check_question=Agent._check_question, _to_internal=Agent._to_internal)
    metadata = {"model": MODEL, "revision": REVISION, "code_revision": CODE_REVISION,
                "sdk_sha256": verified, "encoder_max_position_embeddings": encoder["max_position_embeddings"],
                "checkpoint_max_len": config["max_len"], "checkpoint_head_max_len": config["head_max_len"],
                "tokenizer_sha256": {p.name: hashlib.sha256(p.read_bytes()).hexdigest()
                                     for p in (snapshot / "tokenizer").iterdir() if p.is_file()},
                "model_loads": 0, "model_forwards": 0, "packages": {
                    name: importlib.metadata.version(name) for name in ("laya", "transformers", "tokenizers")}}
    return view, metadata


def audit_request(view, request, ceiling):
    from laya.common import build_sequence, encode_text, render_options, serialize_state
    sys.path.insert(0, str(ROOT / "scripts/benchmarks"))
    from pilot_models import audit_laya_input

    state, questions = request["state"], request["questions"]
    tok = view.tok
    state_ids = encode_text(tok, serialize_state(state).replace(tok.mask_token, " "),
                            add_special_tokens=False)["input_ids"]
    heads = {}
    for name, question in questions.items():
        view._check_question(name, question)
        internal = view._to_internal(question)
        instruction = encode_text(tok, "%s question: %s" % (internal["t"], internal["ins"].replace(tok.mask_token, " ")),
                                  add_special_tokens=False)["input_ids"]
        options = [encode_text(tok, " " + text.replace(tok.mask_token, " "),
                               add_special_tokens=False)["input_ids"] for text in render_options(internal)]
        clipped = [str(key) for key, tokens in zip(question["criteria"], options) if len(tokens) > 48]
        capped_option_tokens = sum(1 + min(48, len(tokens)) for tokens in options)
        head_budget = capped_option_tokens + max(16, len(instruction))
        sequence, markers, stats = build_sequence(tok, state, internal, ceiling, head_budget,
                                                  state_ids=state_ids, return_stats=True)
        # Three separators plus CLS; the actual sequence builder is cross-checked below.
        required = len(instruction) + capped_option_tokens + len(state_ids) + 4
        heads[name] = {"state_tokens": len(state_ids), "instruction_tokens": len(instruction),
                       "options": len(options), "max_option_tokens": max(map(len, options), default=0),
                       "over_48_option_ids": clipped,
                       "option_tokens_lost_at_48": sum(max(0, len(ids) - 48) for ids in options),
                       "head_budget_without_additional_trimming": head_budget,
                       "required_sequence_tokens_after_option_cap": required,
                       "required_sequence_tokens_without_any_loss":
                           len(instruction) + sum(1 + len(tokens) for tokens in options) + len(state_ids) + 4,
                       "actual_sequence_tokens_at_configured_limit": len(sequence),
                       "sequence_sha256": digest(sequence), "markers": len(markers),
                       "options_distinct": stats["options_distinct"],
                       "fits_without_loss": not clipped and required <= ceiling
                           and len(markers) == len(options) and stats["options_distinct"] == len(options)}
        if len(sequence) != min(required, ceiling):
            raise ValueError("Sequence length calculation differs from pinned SDK")
    budgets = [(512, 192), (1024, 512), (2048, 1024), (4096, 2048), (ceiling, 4096)]
    head_budget = max(h["head_budget_without_additional_trimming"] for h in heads.values())
    budgets.append((ceiling, head_budget))
    profiles = []
    for max_len, head_max_len in dict.fromkeys(budgets):
        audit = audit_laya_input(view, state, questions, max_len, head_max_len)
        passed = all(not any(a[k] for k in ("state_tokens_dropped", "head_tokens_dropped", "option_tokens_dropped_at_48"))
                     and a["markers"] == a["options"] == a["options_distinct"] for a in audit.values())
        profiles.append({"max_len": max_len, "head_max_len": head_max_len,
                         "all_heads_lossless": passed, "heads": audit})
    return {"heads": heads, "profiles": profiles, "max_lossless_head_budget": head_budget,
            "max_required_sequence_after_option_cap": max(h["required_sequence_tokens_after_option_cap"] for h in heads.values()),
            "lossless_with_current_sdk_and_encoder": all(h["fits_without_loss"] for h in heads.values())}


def run(captures, output):
    if output.exists():
        raise FileExistsError("Audit output already exists")
    source = json.loads(captures.read_text(encoding="utf-8"))
    expected = json.loads((ROOT / "configs/research/jev-browser-preparation.json").read_text(encoding="utf-8"))
    if source["upstream_revision"] != expected["upstream"]["revision"]:
        raise ValueError("Capture source revision mismatch")
    if [row["task_id"] for row in source["rows"]] != expected["task_set"]["ids"]:
        raise ValueError("Capture must retain all seven tasks, including failed captures")
    view, metadata = load_tokenizer_only()
    result = {"scope": "initial_tokenization_audit_not_model_evaluation", "laya": metadata,
              "audit_script_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "captures_sha256": hashlib.sha256(captures.read_bytes()).hexdigest(),
              "model_calls": 0, "model_loads": 0, "model_forwards": 0, "rows": []}
    for row in source["rows"]:
        audited = {"task_id": row["task_id"], "capture_status": row["status"], "audited": False}
        if row["status"] == "captured":
            if digest(row["request"]) != row["request_sha256"]:
                raise ValueError("Capture hash mismatch")
            audited.update(audited=True, request_sha256=row["request_sha256"],
                           **audit_request(view, row["request"], metadata["encoder_max_position_embeddings"]))
            compact = compact_state_request(row["request"])
            audited["compact_state_proposal"] = {
                "status": "audit_only_not_applied_to_policy_or_worker",
                "request_sha256": digest(compact), "state_roundtrip_equal": True,
                "unchanged_questions_sha256": digest(compact["questions"]),
                **audit_request(view, compact, metadata["encoder_max_position_embeddings"])}
        result["rows"].append(audited)
        print(json.dumps({"task_id": row["task_id"], "audited": audited["audited"],
                          "raw_required": audited.get("max_required_sequence_after_option_cap"),
                          "raw_fits": audited.get("lossless_with_current_sdk_and_encoder"),
                          "compact_required": audited.get("compact_state_proposal", {}).get("max_required_sequence_after_option_cap"),
                          "compact_fits": audited.get("compact_state_proposal", {}).get("lossless_with_current_sdk_and_encoder")}))
    with output.open("x", encoding="utf-8") as handle:
        json.dump(result, handle, indent=2, ensure_ascii=True)
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--captures", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    run(args.captures, args.output)
