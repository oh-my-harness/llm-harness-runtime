"""Tokenizer-only checks; no model parameters, predictions, or public websites."""

from copy import deepcopy
import importlib.util
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "benchmarks/jev_browser"))
from audit_inputs import audit_request, compact_state_request, load_tokenizer_only


class SerializationTests(unittest.TestCase):
    def test_compaction_preserves_every_value_and_question(self):
        request = {"model": "jev-latest", "state": {"page": {"text": 'two  spaces\n"quote" 中文'},
                   "elements": [{"index": "2", "checked": False, "value": None}]},
                   "questions": {"operation": {"criteria": {"CLICK": "Click"}}}}
        before = deepcopy(request)
        compact = compact_state_request(request)
        self.assertEqual(json.loads(compact["state"]), before["state"])
        self.assertEqual(compact["questions"], before["questions"])
        self.assertEqual(request, before)

    def test_preformatted_state_is_not_silently_changed(self):
        with self.assertRaises(ValueError):
            compact_state_request({"state": "unrecognized", "questions": {}})


@unittest.skipUnless(importlib.util.find_spec("laya"), "Use the pinned laya-venv for tokenizer checks")
class TokenizerAuditTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # A regression that loads weights or predicts is a test failure, not an expensive surprise.
        with patch("laya.load", side_effect=AssertionError("No model load")), \
                patch("laya.agent.Agent.__init__", side_effect=AssertionError("No agent instance")), \
                patch("laya.agent.Agent.predict", side_effect=AssertionError("No prediction")), \
                patch("torch.load", side_effect=AssertionError("No weight loading")):
            cls.view, cls.metadata = load_tokenizer_only()

    def request(self, state=None, instructions="Choose an option", criteria=None):
        return {"model": "jev-latest", "state": state or {"text": "Visible content"}, "questions": {
            "operation": {"type": "choice", "instructions": instructions,
                          "criteria": criteria or {"a": "Click", "b": "Wait"}}}}

    def test_metadata_and_small_input(self):
        self.assertEqual(self.metadata["model_loads"], 0)
        self.assertEqual(self.metadata["model_forwards"], 0)
        self.assertEqual(self.metadata["encoder_max_position_embeddings"], 8192)
        result = audit_request(self.view, self.request(), 8192)
        self.assertTrue(result["lossless_with_current_sdk_and_encoder"])

    def test_fixed_option_cap_is_not_hidden_by_wide_budget(self):
        result = audit_request(self.view, self.request(criteria={"a": "detail " * 100, "b": "Wait"}), 8192)
        self.assertEqual(result["heads"]["operation"]["over_48_option_ids"], ["a"])
        self.assertFalse(result["lossless_with_current_sdk_and_encoder"])
        self.assertTrue(all(not p["all_heads_lossless"] for p in result["profiles"]))

    def test_state_overflow_remains_a_failure(self):
        request = self.request(state={"text": "token " * 10000})
        for variant in (request, compact_state_request(request)):
            result = audit_request(self.view, variant, 8192)
            self.assertFalse(result["lossless_with_current_sdk_and_encoder"])
            self.assertGreater(result["max_required_sequence_after_option_cap"], 8192)

    def test_insufficient_head_budget_loses_instructions(self):
        result = audit_request(self.view, self.request(instructions="task instruction " * 150), 8192)
        self.assertFalse(result["profiles"][0]["all_heads_lossless"])
        self.assertGreater(result["profiles"][0]["heads"]["operation"]["head_tokens_dropped"], 0)
        self.assertTrue(result["lossless_with_current_sdk_and_encoder"])


if __name__ == "__main__":
    unittest.main()
