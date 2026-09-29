"""Compatibility checks use tokenizer and reviewed formatting code only."""

from copy import deepcopy
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "benchmarks/jev_browser"))
from audit_browser_checkpoint import MODEL, REVISION, audit_one, load_reviewed_wrapper
from audit_inputs import load_tokenizer_only


class BrowserCheckpointTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.guards = [patch("laya.load", side_effect=AssertionError("No load")),
                      patch("laya.agent.Agent.__init__", side_effect=AssertionError("No agent")),
                      patch("laya.agent.Agent.predict", side_effect=AssertionError("No predict")),
                      patch("torch.load", side_effect=AssertionError("No weights"))]
        for guard in cls.guards:
            guard.start()
            cls.addClassCleanup(guard.stop)
        cls.view, cls.metadata = load_tokenizer_only(model=MODEL, revision=REVISION)
        cls.wrapper = load_reviewed_wrapper()

    def request(self, count=2, text="Visible content"):
        return {"state": {"page": {"url": "https://example.org/", "title": "Test", "text": text},
                          "elements": [], "recent_actions": [], "tabs": []},
                "questions": {"operation": {"type": "choice", "instructions": "Advance goal",
                                             "criteria": {"CLICK": "Click", "DONE": "Done"}},
                              "click_target": {"type": "choice", "instructions": "Select one",
                                               "criteria": {str(i): {"element": "button Option " + str(i)}
                                                            for i in range(count)}}}}

    def test_only_tokenizer_from_selected_checkpoint(self):
        self.assertEqual(self.metadata["model"], MODEL)
        self.assertEqual(self.metadata["revision"], REVISION)
        self.assertEqual(self.metadata["checkpoint_max_len"], 1024)
        self.assertEqual(self.metadata["model_loads"], 0)
        self.assertEqual(self.metadata["model_forwards"], 0)

    def test_author_format_information_loss_is_not_called_lossless(self):
        request = self.request(text="a" * 1300)
        before = deepcopy(request)
        result = audit_one(self.view, self.wrapper, request)
        author = result["author_wrapper_dry_run"]
        self.assertEqual(author["page_text_chars_after"], 1200)
        self.assertEqual(author["page_text_chars_before"], 1300)
        self.assertEqual(author["removed_state_keys"], ["elements", "tabs"])
        self.assertEqual(request, before)
        self.assertTrue(result["complete_input_chunking_proposal"]["all_passes_token_lossless"])

    def test_wide_choice_covers_every_original_candidate(self):
        result = audit_one(self.view, self.wrapper, self.request(count=61))
        proposal = result["complete_input_chunking_proposal"]
        self.assertEqual(proposal["predict_sites_exercised"], 2)
        self.assertTrue(proposal["first_pass_all_candidate_keys_once"])
        self.assertTrue(proposal["all_passes_token_lossless"])

    def test_option_cap_is_still_a_failure(self):
        request = self.request()
        request["questions"]["click_target"]["criteria"]["0"]["element"] = "long detail " * 100
        result = audit_one(self.view, self.wrapper, request)
        self.assertFalse(result["complete_input_chunking_proposal"]["all_passes_token_lossless"])

    def test_missing_or_changed_reviewed_source_is_rejected(self):
        with patch("audit_browser_checkpoint.WRAPPER_SHA256", "0" * 64):
            with self.assertRaisesRegex(ValueError, "hash mismatch"):
                load_reviewed_wrapper()


if __name__ == "__main__":
    unittest.main()
