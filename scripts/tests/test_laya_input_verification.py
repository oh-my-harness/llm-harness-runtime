"""Diagnostic plumbing tests. Model doubles here are not inference evidence."""

import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "benchmarks"))

from pilot_models import Laya
from verify_laya_browser_input import capture_states, digest, expected_sequences, run, verify_sdk_source


class InputVerificationTests(unittest.TestCase):
    def test_default_constructor_still_requires_run_approval(self):
        laya, torch = Mock(), Mock()
        with patch.dict(sys.modules, {"laya": laya, "torch": torch}):
            with self.assertRaisesRegex(ValueError, "awaiting"):
                Laya({"selection_status": "not_approved"})
        laya.load.assert_not_called()
        torch.cuda.is_available.assert_not_called()

    def test_verification_does_not_approve_config_or_allow_cpu(self):
        config = {"selection_status": "not_approved"}
        laya, torch = Mock(), Mock()
        torch.cuda.is_available.return_value = False
        with patch.dict(sys.modules, {"laya": laya, "torch": torch}):
            with self.assertRaisesRegex(RuntimeError, "CPU fallback"):
                Laya(config, input_verification_only=True)
        self.assertEqual(config["selection_status"], "not_approved")
        laya.load.assert_not_called()

    def test_sequence_hashes_keep_question_order_and_sdk_arguments(self):
        agent = Mock()
        agent._to_internal.side_effect = lambda question: question
        agent._encode_state.return_value = [{"ids": [3, 1, 4]}, {"ids": [1, 5]}]
        request = {"state": {"page": "observed"}, "questions": {"operation": {}, "click_target": {}}}
        result = expected_sequences(agent, request, 1024, 512)
        self.assertEqual(result, [{"head": "operation", "tokens": 3, "sha256": digest([3, 1, 4])},
                                  {"head": "click_target", "tokens": 2, "sha256": digest([1, 5])}])
        agent._encode_state.assert_called_once_with(request["state"], ["operation", "click_target"],
                                                    request["questions"], max_len=1024, head_max_len=512)

    def test_existing_results_are_never_overwritten(self):
        with tempfile.TemporaryDirectory() as folder:
            with patch.dict("os.environ", {}, clear=False), patch("verify_laya_browser_input.Laya") as model:
                with self.assertRaises(FileExistsError):
                    run(Path(folder), Path(folder))
                model.assert_not_called()

    def check_source(self, changed):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder)
            for name in ("agent.py", "common.py", "__init__.py"):
                (path / name).write_bytes(b"source\r\n")
            if changed:
                (path / "common.py").write_bytes(b"modified\n")
            distribution = Mock()
            distribution.locate_file.return_value = path
            with patch("verify_laya_browser_input.importlib.metadata.distribution", return_value=distribution), \
                    patch("verify_laya_browser_input.subprocess.run", return_value=Mock(stdout=b"source\n")):
                return verify_sdk_source(path, "pinned-revision")

    def test_source_verification_allows_only_line_ending_difference(self):
        self.assertTrue(all(item["match"] for item in self.check_source(False).values()))

    def test_source_verification_rejects_edited_sdk(self):
        with self.assertRaisesRegex(ValueError, "differs"):
            self.check_source(True)

    @unittest.skipUnless(importlib.util.find_spec("playwright"), "Install the pinned browser test dependency")
    def test_real_observations_do_not_contain_phase_labels_or_checker(self):
        with patch("verify_laya_browser_input.Laya") as model:
            states, info = capture_states()
            model.assert_not_called()
        self.assertEqual([s["phase"] for s in states], ["empty", "name_filled", "form_ready", "saved"])
        self.assertEqual(info["scripted_setup_actions"], 3)
        self.assertEqual(info["model_executed_actions"], 0)
        self.assertTrue(info["fixture_preparation_check"]["success"])
        for captured in states:
            request = captured["request"]
            self.assertEqual(set(request), {"state", "questions"})
            self.assertEqual(set(request["state"]), {"page", "elements", "recent_actions"})
            self.assertEqual(digest(request), captured["request_sha256"])
        self.assertIn("Not saved", states[0]["request"]["state"]["page"]["text"])
        self.assertIn("Saved Demo / Gold", states[-1]["request"]["state"]["page"]["text"])


if __name__ == "__main__":
    unittest.main()
