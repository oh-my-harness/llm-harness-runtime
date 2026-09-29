"""Pinned Laya JSON-lines worker. Never accepts API keys or executes model-selected tools."""

import contextlib
import json
import os
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))


def create_model(config, on_forward):
    from pilot_models import Laya
    model = Laya(config)
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
                body = request["body"]
                if set(body) != {"model", "state", "questions"}:
                    raise ValueError("Unexpected decision payload")
                with contextlib.redirect_stdout(sys.stderr):
                    answers = model.predict(body["state"], body["questions"], event)
                emit({"id": request_id, "ok": True, "answers": answers, "metrics": event})
            else:
                raise ValueError("Unknown worker request")
        except Exception as error:
            emit({"id": request_id, "ok": False, "error_type": type(error).__name__, "metrics": event})


if __name__ == "__main__":
    os.environ["HF_HUB_OFFLINE"] = "1"
    os.environ["TRANSFORMERS_OFFLINE"] = "1"
    serve(sys.stdin, sys.stdout)
