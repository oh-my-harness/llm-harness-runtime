"""Real Python pipe and production protocol, fake inference only."""
import hashlib
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "benchmarks/jev_browser"))
from laya_worker import serve


class FakeModel:
    def __init__(self, config, forward):
        self.forward = forward
        self.setup = {"test_double": True, "stdin_encoding": sys.stdin.encoding,
                      "stdout_encoding": sys.stdout.encoding}

    def predict(self, state, questions, event):
        self.forward()
        event.update(actual_calls=1, forward_attempts=1, decision_questions=len(questions))
        encoded = json.dumps(state, ensure_ascii=False, separators=(",", ":"))
        return {"echo": state, "sha256": hashlib.sha256(encoded.encode("utf-8")).hexdigest()}


serve(sys.stdin, sys.stdout, FakeModel)
