"""Pinned Laya JSON-lines worker. Never accepts API keys or executes model-selected tools."""

import contextlib
import hashlib
import json
import os
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
sys.path.insert(0, str(Path(__file__).resolve().parent))


def create_model(config, on_forward):
    from audit_inputs import load_tokenizer_only
    from pilot_models import Laya
    _, provenance = load_tokenizer_only()
    model = Laya(config)
    model.setup["provenance"] = provenance
    model.agent.model.register_forward_pre_hook(lambda *_: on_forward())
    return model


def serve(reader, writer, factory=create_model):
    def emit(message):
        writer.write(json.dumps(message, ensure_ascii=True) + "\n")
        writer.flush()

    model = None
    active_id = None
    for line in reader:
        request_id, event = None, {}
        try:
            request = json.loads(line)
            request_id = request["id"]
            active_id = request_id
            if request["method"] == "init":
                if model is not None or request.get("allow_model_load") is not True:
                    raise ValueError("Model loading was not enabled")
                config = request["config"]
                if config.get("selection_status") != "approved":
                    raise ValueError("Checkpoint has not been approved")
                with contextlib.redirect_stdout(sys.stderr):
                    model = factory(config, lambda: emit({"id": active_id, "event": "forward_started"}))
                emit({"id": request_id, "ok": True, "setup": model.setup})
            elif request["method"] == "predict":
                if model is None:
                    raise ValueError("Worker is not initialized")
                event.update(actual_calls=0, forward_attempts=0, decision_questions=0)
                if "body_json" in request:
                    wire_hash = hashlib.sha256(request["body_json"].encode("utf-8")).hexdigest()
                    if wire_hash != request.get("body_sha256"):
                        raise ValueError("Input transport checksum mismatch")
                    body = json.loads(request["body_json"])
                    event.update(input_transport_verified=True, wire_sha256=wire_hash)
                else:
                    body = request["body"]
                    event["input_transport_verified"] = False
                if set(body) != {"model", "state", "questions"}:
                    raise ValueError("Unexpected decision payload")
                event["decision_questions"] = len(body["questions"])
                if config.get("state_encoding") == "lossless_compact_json_string":
                    from audit_inputs import compact_state_request, digest
                    original = body
                    body = compact_state_request(original)
                    event.update(state_encoding=config["state_encoding"],
                                 original_request_sha256=digest(original),
                                 encoded_request_sha256=digest(body))
                with contextlib.redirect_stdout(sys.stderr):
                    answers = model.predict(body["state"], body["questions"], event)
                emit({"id": request_id, "ok": True, "answers": answers, "metrics": event})
            else:
                raise ValueError("Unknown worker request")
        except Exception as error:
            emit({"id": request_id, "ok": False, "error_type": type(error).__name__, "metrics": event})


if __name__ == "__main__":
    sys.stdin.reconfigure(encoding="utf-8", errors="strict")
    sys.stdout.reconfigure(encoding="utf-8", errors="strict")
    sys.stderr.reconfigure(encoding="utf-8", errors="backslashreplace")
    os.environ["HF_HUB_OFFLINE"] = "1"
    os.environ["TRANSFORMERS_OFFLINE"] = "1"
    serve(sys.stdin, sys.stdout)
