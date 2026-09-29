"""Native benchmark integration tests; scripted policies never count as model results."""

from copy import deepcopy
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "benchmarks"))

from isolated_browser import IsolatedBrowser
from jev_reference.protocol import build_request
from miniwob_environment import MiniWobEpisode, native_result, serve_checkout, verify_checkout
from run_laya_browser import run_attempt
from run_laya_miniwob import input_hash, schedule, validate_config
from test_browser_policy import ScriptedLaya, ScriptedQwen


ROOT = Path(__file__).resolve().parents[2]
CHECKOUT = ROOT.parent / "benchmark/sources/miniwob-plusplus"
CONFIG = json.loads((ROOT / "configs/research/laya-miniwob-development.json").read_text())


class MiniWobContractTests(unittest.TestCase):
    def test_partial_reward_and_model_done_are_not_success(self):
        for done, reward, expected in ((False, 1, False), (True, 0.8, False), (True, -1, False), (True, 1, True)):
            self.assertEqual(native_result({"done": done, "raw_reward": reward})["success"], expected)
        for reward in (True, float("nan"), "1"):
            with self.assertRaises(ValueError):
                native_result({"done": True, "raw_reward": reward})

    def test_schedule_is_paired_balanced_and_repeatable(self):
        tasks = [{"id": str(i)} for i in range(12)]
        order = schedule(tasks, 20260929)
        self.assertEqual(order, schedule(tasks, 20260929))
        self.assertEqual(len(set(order)), 24)
        self.assertEqual(sum(order[i][2] == "qwen" for i in range(0, 24, 2)), 6)
        for index in range(0, 24, 2):
            self.assertEqual(order[index][0], order[index + 1][0])
            self.assertNotEqual(order[index][2], order[index + 1][2])

    def test_config_cannot_silently_change_task_families_or_grading(self):
        validate_config(CONFIG)
        for key, value in (("families", ["click-button"]), ("success", "positive_reward"), ("mode", "test")):
            changed = deepcopy(CONFIG)
            changed["benchmark"][key] = value
            with self.assertRaises(ValueError):
                validate_config(changed)

    def test_unapproved_config_is_rejected(self):
        config = deepcopy(CONFIG)
        config["run_status"] = "awaiting_approval"
        with self.assertRaises(ValueError):
            validate_config(config)


@unittest.skipUnless(CHECKOUT.is_dir() and importlib.util.find_spec("playwright"), "Pinned local MiniWoB and Chromium required")
class MiniWobBrowserTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        verify_checkout(CHECKOUT)
        cls.server = serve_checkout(CHECKOUT)
        cls.origin = cls.server.__enter__()

    @classmethod
    def tearDownClass(cls):
        cls.server.__exit__(None, None, None)

    def setUp(self):
        self.url = self.origin + "/miniwob/enter-text.html"
        self.browser = IsolatedBrowser(self.url, [self.origin])
        self.episode = MiniWobEpisode("20260929-01", 120)
        self.setup = self.episode.prepare(self.browser)

    def tearDown(self):
        self.browser.close()

    def test_seed_reset_repeats_exact_public_input_and_hides_hud(self):
        page = self.browser.observe(screenshot=False)
        initial = input_hash(build_request(page, self.setup["goal"], []))
        self.assertNotIn("Last reward", page["text"])
        self.assertNotIn("Time left", page["text"])
        self.browser.page.wait_for_timeout(180)
        self.assertTrue(self.browser.fresh(page))
        again = self.episode.prepare(self.browser)
        self.assertEqual(again["goal"], self.setup["goal"])
        self.assertEqual(initial, input_hash(build_request(self.browser.observe(screenshot=False), again["goal"], [])))
        self.assertEqual(again["timeout_ms"], 120000)
        self.assertFalse(self.episode.terminated(self.browser))

    def test_real_wrong_submission_gets_native_failure(self):
        page = self.browser.observe(screenshot=False)
        submit = next(a for a in page["actions"] if a["label"] == "Submit")
        self.browser.act(submit, page)
        self.assertTrue(self.episode.terminated(self.browser))
        result = self.episode.evaluate(self.browser)
        self.assertFalse(result["success"])
        self.assertEqual(result["raw_reward"], -1)

    def test_both_arms_end_on_native_signal_without_model_done(self):
        word = self.setup["goal"].split('"')[1]
        initial = input_hash(build_request(self.browser.observe(screenshot=False), self.setup["goal"], []))
        self.browser.close()
        task = {"id": "native-contract", "family": "enter-text", "seed": "20260929-01", "url": self.url,
                "allowed_origins": [self.origin], "goal": self.setup["goal"], "expected_initial_request_sha256": initial}
        with tempfile.TemporaryDirectory() as folder:
            a = run_attempt(task, 0, "qwen", CONFIG, Path(folder), ScriptedQwen([
                {"operation": "TYPE_TEXT", "target": "1", "text": word},
                {"operation": "CLICK", "target": "2"}]), None, environment=MiniWobEpisode(task["seed"], 120))
            b = run_attempt(task, 0, "qwen_laya", CONFIG, Path(folder), ScriptedQwen([{"text": word}]),
                            ScriptedLaya([("TYPE_TEXT", "1"), ("CLICK", "2")]), environment=MiniWobEpisode(task["seed"], 120))
        for result in (a, b):
            self.assertTrue(result["success"], result)
            self.assertEqual(result["termination"], "environment_done")
            self.assertEqual(result["evaluation"]["raw_reward"], 1)
            self.assertEqual(result["initial_request_sha256"], initial)
            self.assertGreater(result["component_ms"]["environment"], 0)
        self.assertEqual(a["calls"], {"qwen": 2, "laya": 0})
        self.assertEqual(b["calls"], {"qwen": 1, "laya": 2})

    def test_done_and_tampered_initial_hash_cannot_pass(self):
        self.browser.close()
        task = {"id": "early-done", "family": "enter-text", "seed": "20260929-01", "url": self.url,
                "allowed_origins": [self.origin], "goal": self.setup["goal"]}
        with tempfile.TemporaryDirectory() as folder:
            result = run_attempt(task, 0, "qwen", CONFIG, Path(folder),
                                 ScriptedQwen([{"operation": "DONE", "target": None}]), None,
                                 environment=MiniWobEpisode(task["seed"], 120))
            self.assertFalse(result["success"])
            self.assertFalse(result["evaluation"]["done"])
            task.update(id="bad-input", expected_initial_request_sha256="wrong")
            result = run_attempt(task, 0, "qwen", CONFIG, Path(folder), ScriptedQwen([]), None,
                                 environment=MiniWobEpisode(task["seed"], 120))
            self.assertFalse(result["success"])
            self.assertEqual(result["calls"]["qwen"], 0)
            self.assertEqual(result["error_type"], "ValueError")


if __name__ == "__main__":
    unittest.main()
