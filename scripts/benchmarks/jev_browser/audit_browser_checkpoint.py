"""Offline input compatibility audit; no weights, predictions, or live policy changes."""

import argparse
from copy import deepcopy
import hashlib
import importlib.util
import json
from pathlib import Path
import sys
from unittest.mock import patch

from audit_inputs import ROOT, compact_state_request, digest, load_tokenizer_only

MODEL = "cklxx/laya-browser"
REVISION = "645cf366a2ae35f1086e8c20eff48f909bb49206"
WRAPPER_SHA256 = "3dfaba4030a1c96afcd8f5a865a66518e5edb07c6b332e0668f1519a624e6b03"
SNAPSHOT = ROOT.parent / "benchmark/models/huggingface/hub/models--cklxx--laya-browser/snapshots" / REVISION
sys.path.insert(0, str(ROOT / "scripts/benchmarks"))
from pilot_models import audit_laya_input


def load_reviewed_wrapper():
    source = SNAPSHOT / "laya_browser.py"
    if hashlib.sha256(source.read_bytes()).hexdigest() != WRAPPER_SHA256:
        raise ValueError("Reviewed wrapper source hash mismatch")
    spec = importlib.util.spec_from_file_location("reviewed_browser_wrapper", source)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def lossless(heads):
    return all(not any(head[k] for k in ("state_tokens_dropped", "head_tokens_dropped", "option_tokens_dropped_at_48"))
               and head["markers"] == head["options"] == head["options_distinct"] for head in heads.values())


class RecordingAgent:
    """Exercises the wrapper's formatting/splitting only; synthetic winners are NOT model decisions."""

    def __init__(self, view, max_len=1024, head_max_len=768):
        self.view = view
        self.max_len = max_len
        self.head_max_len = head_max_len
        self.calls = []

    def predict(self, state, questions):
        from laya.common import encode_text, render_options
        heads = audit_laya_input(self.view, state, questions, self.max_len, self.head_max_len)
        self.calls.append({"state": deepcopy(state), "questions": deepcopy(questions), "heads": heads,
                           "all_heads_lossless": lossless(heads)})
        answers = {}
        for name, question in questions.items():
            keys = list(question["criteria"])
            options = render_options(self.view._to_internal(question))
            sizes = [len(encode_text(self.view.tok, " " + option.replace(self.view.tok.mask_token, " "),
                                     add_special_tokens=False)["input_ids"]) for option in options]
            # Longest options stress the finalists' budget, without consulting correctness labels.
            choice = keys[max(range(len(keys)), key=lambda i: min(48, sizes[i]))]
            answers[name] = {"type": "choice", "choice": choice, "confidence": 0,
                             "probabilities": {key: float(key == choice) for key in keys}}
        return {"answers": answers}


def audit_one(view, wrapper, request):
    compact = compact_state_request(request)
    raw_profiles = []
    for max_len, head_max_len in ((1024, 768), (8192, 4096)):
        heads = audit_laya_input(view, compact["state"], compact["questions"], max_len, head_max_len)
        raw_profiles.append({"max_len": max_len, "head_max_len": head_max_len,
                             "all_heads_lossless": lossless(heads), "heads": heads})
    recorder = RecordingAgent(view)
    wrapper.LayaBrowser(recorder).systemone(deepcopy(request))
    complete_recorder = RecordingAgent(view, 8192, 4096)
    wrapper.predict_chunked(complete_recorder, compact["state"], deepcopy(compact["questions"]))
    first_questions = complete_recorder.calls[0]["questions"]
    for name, question in compact["questions"].items():
        parts = [q for key, q in first_questions.items() if key == name or key.startswith(name + "__chunk")]
        seen = [key for part in parts for key in part["criteria"]]
        if len(seen) != len(set(seen)) or set(seen) != set(question["criteria"]):
            raise ValueError("Candidate loss or duplication in chunking")
        if any(part["instructions"] != question["instructions"] for part in parts):
            raise ValueError("Changed chunk instructions")
        for part in parts:
            if any(value != question["criteria"][key] for key, value in part["criteria"].items()):
                raise ValueError("Changed candidate content")
    if any(call["state"] != compact["state"] for call in complete_recorder.calls):
        raise ValueError("Changed complete state")
    formatted = recorder.calls[0]["state"]
    original_state = request["state"]
    max_options = max(len(q["criteria"]) for q in request["questions"].values())
    return {"request_sha256": digest(request), "complete_input_profiles": raw_profiles,
            "complete_input_chunking_proposal": {
                "status": "audit_only_not_applied_to_policy_or_worker",
                "max_len": 8192, "head_max_len": 4096, "model_forwards": 0,
                "first_pass_all_candidate_keys_once": True,
                "state_instructions_and_candidate_content_unchanged": True,
                "predict_sites_exercised": len(complete_recorder.calls),
                "all_passes_token_lossless": all(call["all_heads_lossless"] for call in complete_recorder.calls),
                "passes": [{"all_heads_lossless": call["all_heads_lossless"], "heads": call["heads"]}
                           for call in complete_recorder.calls]},
            "author_wrapper_dry_run": {
                "scope": "formatting_and_longest_option_synthetic_winners_not_predictions",
                "model_forwards": 0, "predict_sites_exercised": len(recorder.calls),
                "max_original_options": max_options,
                "page_text_chars_before": len(original_state["page"]["text"]),
                "page_text_chars_after": len(formatted["page"]["text"]),
                "removed_state_keys": sorted(set(original_state) - set(formatted)),
                "questions_before": len(request["questions"]),
                "first_pass_questions": len(recorder.calls[0]["questions"]),
                "all_passes_token_lossless": all(call["all_heads_lossless"] for call in recorder.calls),
                "passes": [{"all_heads_lossless": call["all_heads_lossless"], "heads": call["heads"]}
                           for call in recorder.calls]}}


def run(captures, traces, output):
    if output.exists():
        raise FileExistsError("Refuse to overwrite audit")
    source = json.loads(captures.read_text(encoding="utf-8"))
    config = json.loads((ROOT / "configs/research/jev-browser-preparation.json").read_text(encoding="utf-8"))
    if source["upstream_revision"] != config["upstream"]["revision"]:
        raise ValueError("Capture revision mismatch")
    if [row["task_id"] for row in source["rows"]] != config["task_set"]["ids"]:
        raise ValueError("Expected all seven tasks")
    requests = []
    for row in source["rows"]:
        if row["status"] != "captured" or digest(row["request"]) != row["request_sha256"]:
            raise ValueError("Capture status or hash mismatch")
        requests.append(("initial:" + row["task_id"], row["request"]))
    evidence = {captures.name: hashlib.sha256(captures.read_bytes()).hexdigest()}
    trace_files = sorted(traces.glob("*-B.jsonl"))
    if sorted(file.name.split("-", 1)[1][:-8] for file in trace_files) != sorted(config["task_set"]["ids"]):
        raise ValueError("Expected one B trace for every task")
    for file in trace_files:
        evidence[file.name] = hashlib.sha256(file.read_bytes()).hexdigest()
        for row in map(json.loads, file.read_text(encoding="utf-8").splitlines()):
            if row.get("component") == "laya" and row.get("phase") == "end":
                if row["status"] != "ok" or digest(row["request"]) != row["original_request_sha256"]:
                    raise ValueError("Trace status or hash mismatch")
                requests.append((file.name + ":" + str(row["id"]), row["request"]))
    with patch("laya.load", side_effect=AssertionError("No model load")), \
            patch("laya.agent.Agent.__init__", side_effect=AssertionError("No model construction")), \
            patch("laya.agent.Agent.predict", side_effect=AssertionError("No model prediction")), \
            patch("torch.load", side_effect=AssertionError("No weights")):
        view, metadata = load_tokenizer_only(model=MODEL, revision=REVISION)
        wrapper = load_reviewed_wrapper()
        rows = [{"id": name, **audit_one(view, wrapper, request)} for name, request in requests]
    result = {"scope": "offline_compatibility_not_quality_or_speed", "model_calls": 0,
              "laya": metadata, "wrapper_sha256": WRAPPER_SHA256, "evidence_sha256": evidence,
              "script_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), "rows": rows}
    with output.open("x", encoding="utf-8") as handle:
        json.dump(result, handle, ensure_ascii=True, indent=2)
    for row in rows:
        author = row["author_wrapper_dry_run"]
        print(json.dumps({"id": row["id"], "complete_input_lossless":
                          [p["all_heads_lossless"] for p in row["complete_input_profiles"]],
                          "wrapper_token_lossless": author["all_passes_token_lossless"],
                          "complete_chunking_lossless": row["complete_input_chunking_proposal"]["all_passes_token_lossless"],
                          "page_chars": [author["page_text_chars_before"], author["page_text_chars_after"]],
                          "predict_sites_exercised": author["predict_sites_exercised"]}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--captures", type=Path, required=True)
    parser.add_argument("--traces", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    run(args.captures, args.traces, args.output)
