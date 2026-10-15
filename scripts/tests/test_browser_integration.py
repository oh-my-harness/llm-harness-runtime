"""Real local Chromium, scripted policy doubles. No provider calls or benchmark claims."""

from copy import deepcopy
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
from pathlib import Path
import sys
import tempfile
import threading
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "benchmarks"))

from isolated_browser import IsolatedBrowser, evaluate
from jev_reference.browser import StalePage
from jev_reference.protocol import action_space
from run_laya_browser import run_attempt
from test_browser_policy import CONFIG, ScriptedLaya, ScriptedQwen


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        content = Path(__file__).with_name("fixtures").joinpath("browser_contract.html").read_bytes()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(content)))
        self.end_headers()
        self.wfile.write(content)

    def log_message(self, *_args):
        pass


@unittest.skipUnless(importlib.util.find_spec("playwright"), "Install the pinned browser test dependency")
class BrowserIntegrationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        cls.origin = "http://127.0.0.1:" + str(cls.server.server_port)
        cls.url = cls.origin + "/"

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join()

    def setUp(self):
        self.browser = IsolatedBrowser(self.url, [self.origin])

    def tearDown(self):
        self.browser.close()

    def test_observed_actions_exclude_unsafe_and_invisible_controls(self):
        page = self.browser.observe(screenshot=False)
        labels = [a["label"] for a in page["actions"]]
        for label in ("Password", "Upload", "Disabled", "Hidden button"):
            self.assertNotIn(label, labels)
        self.assertFalse(any(a["label"] == "Read only" and a["kind"] == "fill" for a in page["actions"]))
        self.assertEqual(page["omitted_actions"], 0)

    def test_fill_select_click_and_independent_outcome(self):
        spec = {"visible_text": ["Saved Demo / Gold"], "fields": [{"selector": "#name", "value": "Demo"},
                                                                 {"selector": "#tier", "value": "gold"}]}
        self.assertFalse(evaluate(self.browser.page, spec)["success"])
        for kind, label, value in (("fill", "Name", "Demo"), ("select", "Tier → Gold", None), ("click", "Save", None)):
            page = self.browser.observe(screenshot=False)
            action = next(a for a in page["actions"] if a["kind"] == kind and a["label"] == label)
            self.browser.act(action, page, text=value)
        self.browser.observe(screenshot=False)
        self.assertTrue(evaluate(self.browser.page, spec)["success"])
        self.assertEqual(self.browser.page.evaluate("window.submissions"), 1)

    def test_changed_target_rejected_before_click(self):
        page = self.browser.observe(screenshot=False)
        action = next(a for a in page["actions"] if a["label"] == "Save")
        self.browser.page.locator("#save").evaluate("e => e.textContent = 'Changed'")
        with self.assertRaises(StalePage):
            self.browser.act(action, page)
        self.assertEqual(self.browser.page.evaluate("window.submissions"), 0)

    def test_occluded_target_rejected_before_click(self):
        page = self.browser.observe(screenshot=False)
        action = next(a for a in page["actions"] if a["label"] == "Save")
        self.browser.page.evaluate("""() => { const e = document.createElement('div');
            e.style = 'position:fixed;inset:0;z-index:999;background:white'; document.body.append(e); }""")
        with self.assertRaises(StalePage):
            self.browser.act(action, page)
        self.assertEqual(self.browser.page.evaluate("window.submissions"), 0)

    def test_hidden_success_text_does_not_pass(self):
        self.browser.page.locator("#result").evaluate("e => {e.textContent='Saved Demo / Gold';e.hidden=true}")
        self.assertFalse(evaluate(self.browser.page, {"visible_text": ["Saved Demo / Gold"]})["success"])

    def test_network_outside_allowlist_blocked(self):
        result = self.browser.page.evaluate("fetch('http://127.0.0.1:1/outside').then(() => true).catch(() => false)")
        self.assertFalse(result)
        self.assertEqual(self.browser.blocked_requests, 1)

    def test_fresh_context_has_no_previous_cookie_or_storage(self):
        self.browser.page.evaluate("localStorage.setItem('test','previous');document.cookie='session=previous'")
        # End the first owned browser before starting a second sync Playwright driver.
        self.browser.close()
        self.browser = IsolatedBrowser(self.url, [self.origin])
        self.assertIsNone(self.browser.page.evaluate("localStorage.getItem('test')"))
        self.assertEqual(self.browser.context.cookies(), [])

    def test_scroll_and_wait_are_supported_observed_operations(self):
        self.browser.page.evaluate("document.body.style.height = '2400px'")
        page = self.browser.observe(screenshot=False)
        _, _, controls = action_space(page["actions"])
        self.assertIn("SCROLL_DOWN", controls)
        self.browser.act(controls["SCROLL_DOWN"], page)
        self.browser.page.wait_for_function("scrollY > 0")
        page = self.browser.observe(screenshot=False)
        _, _, controls = action_space(page["actions"])
        self.assertIn("SCROLL_UP", controls)
        self.browser.act(controls["WAIT"], page)

    def test_both_wirings_drive_actual_browser_with_test_doubles(self):
        self.browser.close()
        task = {"id": "offline-contract", "url": self.url, "goal": "Enter Demo, select Gold, save, and stop.",
                "allowed_origins": [self.origin], "reset_policy": "fresh_browser_context_client_state_only",
                "acceptance": {"visible_text": ["Saved Demo / Gold"]}}
        with tempfile.TemporaryDirectory() as folder:
            a = run_attempt(task, 0, "qwen", CONFIG, Path(folder), ScriptedQwen([
                {"operation": "TYPE_TEXT", "target": "1", "text": "Demo"},
                {"operation": "SELECT", "target": "2:1"}, {"operation": "CLICK", "target": "3"},
                {"operation": "DONE", "target": None}]), None)
            b = run_attempt(deepcopy(task), 0, "qwen_laya", CONFIG, Path(folder), ScriptedQwen([{"text": "Demo"}]),
                            ScriptedLaya([("TYPE_TEXT", "1"), ("SELECT", "2:1"), ("CLICK", "3"), ("DONE", None)]))
        self.assertTrue(a["success"], a)
        self.assertTrue(b["success"], b)
        self.assertEqual(a["calls"], {"qwen": 4, "laya": 0})
        self.assertEqual(b["calls"], {"qwen": 1, "laya": 4})
        self.assertEqual(a["qwen_by_purpose"], {"mixed": 1, "decision": 3})
        self.assertEqual(b["qwen_by_purpose"], {"generation": 1})
        self.assertEqual(a["actions_executed"], b["actions_executed"])


if __name__ == "__main__":
    unittest.main()
