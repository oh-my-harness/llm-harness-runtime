"""Pinned Decider scoring behind the unchanged author's browser request wrapper."""

import hashlib
import json
from pathlib import Path
import sys
import time

REVISION = "533964dae8be954c5b5e19fa4948e48408094c1e"
WEIGHT_SHA256 = "acaef2228b134dcdc20cad4ee79219482c927ec819aa3687b9b8a575c338817f"
ROOT = Path(__file__).resolve().parents[3]
SNAPSHOT = ROOT.parent / "benchmark/models/huggingface/hub/models--Mapika--decider-2b/snapshots" / REVISION
MAX_STATE_TOKENS = 32768


def file_sha(path):
    with path.open("rb") as handle:
        return hashlib.file_digest(handle, "sha256").hexdigest()


def schedule(tasks, repeat_index=0):
    result = []
    for index, task in enumerate(tasks):
        arms = ["A", "B"] if (index + repeat_index) % 2 == 0 else ["B", "A"]
        arms.insert((index + repeat_index) % 3, "C")
        result.extend((task, arm) for arm in arms)
    return result


def validate_answers(questions, result):
    answers = result.get("answers", {})
    if set(answers) != set(questions):
        raise ValueError("Decider answer ids do not match request")
    for key, question in questions.items():
        answer = answers[key]
        candidates = question["criteria"]
        probabilities = answer.get("probabilities", {})
        if answer.get("choice") not in candidates or set(probabilities) != set(candidates):
            raise ValueError("Decider returned an unavailable candidate")
        import math
        if any(not isinstance(p, (int, float)) or not math.isfinite(p) or not 0 <= p <= 1
               for p in probabilities.values()) or abs(sum(probabilities.values()) - 1) > .01:
            raise ValueError("Decider returned invalid probabilities")
    return result


def render_choice(question, render_question):
    if question.get("type") != "choice" or not isinstance(question.get("criteria"), dict):
        raise ValueError("Browser adapter requires named choice candidates")
    criteria = question["criteria"]
    if len(criteria) != 1:
        return render_question(question)
    # The upstream renderer rejects width one, but its prompt and scoring head support it.
    # Keep the actual row and forward pass; do not invent a second candidate or bypass inference.
    name, description = next(iter(criteria.items()))
    instruction = question.get("instructions", "")
    if not instruction:
        raise ValueError("Question requires instructions")
    text = lambda value: value if isinstance(value, str) else json.dumps(value, ensure_ascii=False)
    option = name if description in (None, "") else f"{name}: {text(description)}"
    return {"question": text(instruction), "options": [option], "type": "choice", "names": [name],
            "legend": None, "isolated": bool(question.get("isolated", True))}


class DeciderPredictAdapter:
    def __init__(self, emit=lambda *args, **kwargs: None, forward=lambda: None):
        import torch
        if not torch.cuda.is_available():
            raise RuntimeError("CUDA is required; no CPU fallback")
        if file_sha(SNAPSHOT / "model.safetensors") != WEIGHT_SHA256:
            raise ValueError("Decider weight hash mismatch")
        sys.path.insert(0, str(SNAPSHOT))
        from decider.infer import Decider
        import decider.infer
        if not Path(decider.infer.__file__).resolve().is_relative_to(SNAPSHOT.resolve()):
            raise ValueError("Unexpected Decider inference source")
        cudnn = torch.backends.cuda.cudnn_sdp_enabled()
        try:
            self.native = Decider(str(SNAPSHOT), device="cuda", dtype=torch.bfloat16, use_graphs=False)
        finally:
            torch.backends.cuda.enable_cudnn_sdp(cudnn)
        self.emit = emit
        self.device = self.native.dev
        self.native.m.lm.model.register_forward_pre_hook(lambda module, args: forward())
        self.metadata = {
            "repo": "Mapika/decider-2b", "revision": REVISION, "version": self.native.name,
            "parameters": sum(p.numel() for p in self.native.m.parameters()), "device": self.device,
            "dtype": "bfloat16", "use_graphs": False, "schema_cache": False,
            "max_state_tokens": MAX_STATE_TOKENS, "singleton_rows_scored": True,
            "files_sha256": {str(p.relative_to(SNAPSHOT)): file_sha(p) for p in sorted(SNAPSHOT.rglob("*"))
                             if p.is_file() and (p.suffix in (".py", ".json") or p.name == "model.safetensors")},
        }

    def items(self, state, questions):
        from decider.infer import Example, Q, _NoShuffle
        from decider.prompt import build, MAX_OPTIONS
        from decider.systemone import render_state, render_question, plan_rows, row_types
        context = render_state(state)
        if len(self.native.m.tok.encode("Context:\n" + context, add_special_tokens=False)) > MAX_STATE_TOKENS:
            raise ValueError("Decider context would be truncated")
        rendered = {key: render_choice(q, render_question) for key, q in questions.items()}
        flat, index = plan_rows(rendered, True)
        items = [build(Example(context, [Q(row["question"], row["options"])]), self.native.m.tok,
                       _NoShuffle(), max_options=MAX_OPTIONS, max_ctx_tokens=MAX_STATE_TOKENS,
                       layout="state_first", chat=None) for row in flat]
        for item, kind in zip(items, row_types(rendered, index)):
            item["types"] = [kind]
            if len(item["ids"]) > MAX_STATE_TOKENS:
                raise ValueError("Decider complete prompt exceeds the frozen input budget")
        return rendered, index, items

    def predict(self, state, questions):
        from decider import temperature as TT
        from decider.systemone import assemble, unique_tokens
        from torch.nn.attention import sdpa_kernel, SDPBackend
        started = time.perf_counter()
        rendered, index, items = self.items(state, questions)
        self.emit("decider_input", input_sha256=hashlib.sha256(json.dumps(
            {"state": state, "questions": questions}, ensure_ascii=False, sort_keys=True).encode()).hexdigest(),
            row_tokens=[len(item["ids"]) for item in items],
            candidates={key: list(q["criteria"]) for key, q in questions.items()},
            singleton_rows=sum(len(q["criteria"]) == 1 for q in questions.values()))
        # Apply the author's Blackwell attention workaround only to C, preserving B's backend state.
        with sdpa_kernel([SDPBackend.FLASH_ATTENTION, SDPBackend.EFFICIENT_ATTENTION, SDPBackend.MATH]):
            probabilities = self.native._system_one_probs(
                items, "state_first", 65536, TT.for_items(self.native.T, self.native.T_by_type, items))
        result = validate_answers(questions, {"model": self.native.name,
            "answers": assemble(rendered, index, probabilities),
            "usage": {"input_tokens": unique_tokens(items), "output_tokens": 0}})
        self.emit("decider_predict_result", elapsed_ms=(time.perf_counter() - started) * 1000,
                  usage=result["usage"], answers=result["answers"])
        return result


def compare(rows):
    import author_control
    result = {}
    for left, right in (("A", "B"), ("A", "C"), ("B", "C")):
        mapped = [{**row, "arm": "A" if row["arm"] == left else "B"}
                  for row in rows if row["arm"] in (left, right)]
        result[f"{left}_vs_{right}"] = {"left": left, "right": right,
                                         **author_control.compare(mapped)}
    return result


if __name__ == "__main__":
    import argparse
    import os
    parser = argparse.ArgumentParser(description="Offline CUDA and protocol preflight; no browser or API calls")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    os.environ.update(HF_HUB_OFFLINE="1", TRANSFORMERS_OFFLINE="1")
    calls = []
    adapter = DeciderPredictAdapter(forward=lambda: calls.append(1))
    state = {"page": "A submit button and a search box"}
    questions = {"operation": {"type": "choice", "instructions": "Choose the action to submit.",
                              "criteria": {"CLICK": "Press a button", "TYPE_TEXT": "Fill a field"}}}
    from torch.nn.attention import sdpa_kernel, SDPBackend
    with sdpa_kernel([SDPBackend.FLASH_ATTENTION, SDPBackend.EFFICIENT_ATTENTION, SDPBackend.MATH]):
        native = adapter.native.system_one(state, questions)
    adapted = adapter.predict(state, questions)
    if native != adapted:
        raise RuntimeError("Adapter differs from the native multi-candidate scoring path")
    singleton = {"target": {"type": "choice", "instructions": "Select the only offered target.",
                            "criteria": {"1": "Submit button"}}}
    before = len(calls)
    one = adapter.predict(state, singleton)
    if len(calls) <= before or one["answers"]["target"]["probabilities"] != {"1": 1.0}:
        raise RuntimeError("Singleton scoring skipped the model or changed candidates")
    from author_suite import SNAPSHOT as LAYA_SNAPSHOT
    sys.path.insert(0, str(LAYA_SNAPSHOT / "code/apps"))
    from laya_browser import LayaBrowser
    browser = LayaBrowser(adapter)
    sample = json.loads((LAYA_SNAPSHOT / "code/sample_request.json").read_text(encoding="utf-8"))
    timings = []
    for _ in range(3):
        started = time.perf_counter()
        result = browser.systemone(sample)
        timings.append(time.perf_counter() - started)
        validate_answers(sample["questions"], result)
    report = {"metadata": adapter.metadata, "native_parity": True, "singleton_forward": True,
              "sample_seconds": timings, "forwards": len(calls), "sample_answers": result["answers"]}
    (args.output / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps({key: value for key, value in report.items() if key != "metadata"}), flush=True)
