"""Offline protocol tests; the factory never imports torch or loads weights."""

import importlib.util
import hashlib
import io
import json
from pathlib import Path
import unittest

WORKER = Path(__file__).resolve().parents[1] / "benchmarks/jev_browser/laya_worker.py"
SPEC = importlib.util.spec_from_file_location("jev_laya_worker", WORKER)
worker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(worker)


class FakeLaya:
    setup = {"test_double": True, "warmup": []}

    def __init__(self, config, on_forward):
        self.on_forward = on_forward

    def predict(self, state, questions, event):
        event.update(actual_calls=0, forward_attempts=0)
        if state == "reject-before-forward":
            raise ValueError("Input budget would truncate")
        self.on_forward()
        event.update(actual_calls=1, forward_attempts=1, decision_questions=len(questions))
        return {"operation": {"choice": "DONE"}}


def invoke(requests, factory=FakeLaya):
    reader = io.StringIO("\n".join(json.dumps(r) for r in requests))
    writer = io.StringIO()
    worker.serve(reader, writer, factory)
    return [json.loads(line) for line in writer.getvalue().splitlines()]


def init():
    return {"id": 1, "method": "init", "allow_model_load": True,
            "config": {"selection_status": "approved"}}


def predict(state="page"):
    return {"id": 2, "method": "predict", "body": {
        "model": "jev-latest", "state": state, "questions": {"operation": {}}}}


class WorkerTests(unittest.TestCase):
    def test_wire_checksum_rejects_changed_input_before_inference(self):
        body = json.dumps(predict()["body"])
        request = {"id": 2, "method": "predict", "body_json": body,
                   "body_sha256": hashlib.sha256((body + " ").encode()).hexdigest()}
        result = invoke([init(), request])[-1]
        self.assertFalse(result["ok"])
        self.assertEqual(result["metrics"]["actual_calls"], 0)
        self.assertEqual(result["metrics"]["forward_attempts"], 0)

    def test_live_encoding_preserves_structured_state_and_all_candidates(self):
        state = {"goal": "保留完整内容", "page": {"text": " a  b \n c "}}
        request = init()
        request["config"]["state_encoding"] = "lossless_compact_json_string"
        seen = []
        class Inspect(FakeLaya):
            def predict(self, encoded, questions, event):
                seen.append((encoded, questions))
                return super().predict(encoded, questions, event)
        result = invoke([request, predict(state)], Inspect)
        self.assertTrue(result[-1]["ok"])
        self.assertEqual(json.loads(seen[0][0]), state)
        self.assertEqual(seen[0][1], {"operation": {}})
        self.assertIn("encoded_request_sha256", result[-1]["metrics"])

    def test_no_implicit_loading(self):
        calls = []
        result = invoke([predict()], lambda *args: calls.append(args))
        self.assertFalse(result[0]["ok"])
        self.assertEqual(calls, [])

    def test_unapproved_config_cannot_load(self):
        request = init()
        request["config"]["selection_status"] = "pending"
        self.assertFalse(invoke([request])[0]["ok"])

    def test_explicit_loading_gate(self):
        request = init()
        request["allow_model_load"] = False
        self.assertFalse(invoke([request])[0]["ok"])

    def test_forward_notification_precedes_completed_result(self):
        results = invoke([init(), predict()])
        self.assertTrue(results[0]["setup"]["test_double"])
        self.assertEqual(results[1], {"id": 2, "event": "forward_started"})
        self.assertTrue(results[2]["ok"])
        self.assertEqual(results[2]["metrics"]["forward_attempts"], 1)
        self.assertEqual(results[2]["metrics"]["actual_calls"], 1)

    def test_preflight_rejection_is_zero_forwards_not_a_prediction(self):
        results = invoke([init(), predict("reject-before-forward")])
        self.assertEqual(len(results), 2)
        self.assertFalse(results[1]["ok"])
        self.assertEqual(results[1]["metrics"]["actual_calls"], 0)
        self.assertEqual(results[1]["metrics"]["forward_attempts"], 0)

    def test_errors_do_not_echo_provider_or_input_content(self):
        def fail(*args):
            raise ValueError("private-placeholder")
        result = invoke([init()], fail)
        self.assertNotIn("private-placeholder", json.dumps(result))
        self.assertEqual(result[0]["error_type"], "ValueError")

    def test_reinitialization_is_refused(self):
        self.assertFalse(invoke([init(), init()])[1]["ok"])

    def test_extra_prediction_payload_keys_are_refused(self):
        request = predict()
        request["body"]["answer"] = "leaked"
        self.assertFalse(invoke([init(), request])[1]["ok"])


if __name__ == "__main__":
    unittest.main()
