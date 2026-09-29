"""Offline checks for a fair, bounded same-engine Qwen control."""

import importlib.util
import json
import os
from pathlib import Path
import re
import sys
from types import SimpleNamespace
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / "benchmarks/jev_browser/author_control.py"
SPEC = importlib.util.spec_from_file_location("author_control", SCRIPT)
control = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(control)
sys.path.insert(0, str(SCRIPT.parent))
import audit_author_run


class ControlTests(unittest.TestCase):
    def test_null_guard_preserves_exclusions(self):
        agent = SimpleNamespace(state={"page": {"actions": [{"id": "e1"}, {"id": "e2"}]},
                                       "excluded": {"e1"}}, _undo_guard=lambda: None)
        with patch.dict(os.environ, {"JEV_SAFE": "0"}):
            self.assertEqual(control.fixed_decision_page(agent, re.compile("pay"))["actions"], [{"id": "e2"}])

    def test_active_undo_guard_is_not_disabled(self):
        agent = SimpleNamespace(state={"page": {"actions": [{"id": "e1"}, {"id": "e2"}]}},
                                _undo_guard=lambda: lambda a: a["id"] == "e2")
        with patch.dict(os.environ, {"JEV_SAFE": "0"}):
            self.assertEqual(control.fixed_decision_page(agent, re.compile("pay"))["actions"], [{"id": "e1"}])

    def test_no_guard_preserves_original_page(self):
        page = {"actions": []}
        agent = SimpleNamespace(state={"page": page}, _undo_guard=lambda: None)
        with patch.dict(os.environ, {"JEV_SAFE": "0"}):
            self.assertIs(control.fixed_decision_page(agent, re.compile("pay")), page)

    def test_parse_rejects_invented_or_excluded_actions(self):
        targets = {"CLICK": {"1": {"id": "e1"}}}
        for value in ({"operation": "CLICK", "target": "2", "text": None},
                      {"operation": "TYPE_TEXT", "target": "1", "text": "x"},
                      {"operation": "DONE", "target": "1", "text": None},
                      {"operation": "CLICK", "target": "1", "text": "extra"}):
            with self.assertRaises(ValueError):
                control.parse_output(json.dumps(value), targets, {})
        with self.assertRaises(ValueError):
            control.parse_output('{"operation":"DONE","target":null,"text":null}', targets, {}, ("DONE",))

    def test_select_maps_to_specific_observed_option(self):
        action = {"id": "select-2", "kind": "select"}
        parsed = control.parse_output('{"operation":"SELECT","target":"1:2","text":null}',
                                      {"SELECT": {"1:2": action}}, {})
        self.assertIs(parsed[3], action)

    def test_combined_text_uses_one_http_and_no_helper_call(self):
        calls = []
        action = {"id": "field", "kind": "fill", "label": "Search"}
        context = {"field": {"label": "Search"}, "goal": "Find a book"}
        def post(*args):
            calls.append(args)
            return {"choices": [{"message": {"content": '{"operation":"TYPE_TEXT","target":"1","text":"book"}'}}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 4}}
        model = SimpleNamespace(action_space=lambda _: ([{"index": "1"}], {"TYPE_TEXT": {"1": action}}, {}),
                                field_context=lambda *args: context, post_json=post)
        policy = control.QwenControl(model, "Only observed actions", lambda *args, **kw: None)
        with patch.dict(os.environ, {"TEXT_MODEL": "qwen", "TEXT_MODEL_BASE_URL": "https://example.invalid/v1",
                                     "TEXT_MODEL_API_KEY": "test-only"}):
            result = policy.choose({"url": "https://example.invalid", "title": "t", "text": "x", "actions": [action]},
                                   "Find a book", [])
            text, meta = policy.field_text(context)
        self.assertEqual(len(calls), 1)
        self.assertEqual(text, "book")
        self.assertTrue(meta["merged"])
        self.assertEqual(result["choice"], "field")
        with self.assertRaises(ValueError):
            policy.field_text({"different": "context"})

    def test_schedule_balances_order_and_runs_every_task_twice(self):
        tasks = [("one",), ("two",), ("three",), ("four",)]
        schedule = control.schedule(tasks)
        self.assertEqual([arm for _, arm in schedule], ["A", "B", "B", "A", "A", "B", "B", "A"])
        self.assertEqual(len(schedule), 8)

    def test_checkbox_false_and_runtime_errors_fail(self):
        self.assertFalse(control.accepted(True, "done", "internet-checkbox", {"actions": [{"role": "checkbox", "checked": "false"}]}))
        self.assertTrue(control.accepted(True, "done", "internet-checkbox", {"actions": [{"role": "checkbox", "checked": "true"}]}))
        self.assertFalse(control.accepted(True, "error:TypeError", "any-task", {}))

    def test_failed_fast_run_does_not_count_as_speedup(self):
        rows = [dict(task="one", arm="A", **{"pass": True}, attempt_wall_seconds=10, counts={"qwen_http": 2}),
                dict(task="one", arm="B", **{"pass": False}, attempt_wall_seconds=.01, counts={"qwen_http": 0})]
        result = control.compare(rows)
        self.assertIsNone(result["paired_speed_ratio_A_over_B"])
        self.assertEqual(result["quality_pairs"]["only_A"], 1)
        rows += [dict(task="two", arm="A", **{"pass": True}, attempt_wall_seconds=6, counts={"qwen_http": 3}),
                 dict(task="two", arm="B", **{"pass": True}, attempt_wall_seconds=3, counts={"qwen_http": 1})]
        self.assertEqual(control.compare(rows)["paired_speed_ratio_A_over_B"], 2)

    def test_incomplete_pairs_and_unknown_tokens_are_preserved(self):
        row = dict(task="one", arm="A", **{"pass": False}, attempt_wall_seconds=3, counts={"qwen_http": 1, "tokens": None})
        result = control.compare([row])
        self.assertEqual(result["quality_pairs"]["incomplete"], 1)
        self.assertIsNone(result["groups"]["A"]["counts"]["tokens"])

    def test_usage_audit_keeps_unknown_requests_separate_from_other_arms(self):
        a = [{"component": "qwen_http", "usage": {"prompt_tokens": 5, "completion_tokens": 2}},
             {"component": "qwen_http", "status": "error"}]
        b = [{"component": "qwen_http", "usage": {"prompt_tokens": 3, "completion_tokens": 1}}]
        self.assertIsNone(audit_author_run.usage_summary(a)["prompt_tokens"]["total"])
        self.assertEqual(audit_author_run.usage_summary(a)["prompt_tokens"]["known_sum"], 5)
        self.assertEqual(audit_author_run.usage_summary(b)["prompt_tokens"]["total"], 3)
        self.assertEqual(audit_author_run.usage_summary([])["prompt_tokens"]["total"], 0)

    def test_real_author_loop_consumes_merged_text_without_extra_http(self):
        engine = SCRIPT.parents[4] / "benchmark/sources/jev-ultrafast"
        if not engine.exists():
            self.skipTest("Pinned author engine not installed")
        sys.path.insert(0, str(engine))
        import jev_ultrafast.agent as agent
        import jev_ultrafast.model as model
        class Browser:
            def __init__(self, url):
                self.page = {"url": url, "title": "test", "text": "form", "fingerprint": "before",
                             "actions": [{"id": "field", "node": 1, "kind": "fill", "label": "Search", "role": "textbox", "value": ""}]}
                self.typed = []
            def observe(self, **kwargs):
                return self.page
            def fresh(self, page):
                return True
            def act(self, action, page, text=None):
                self.typed.append(text)
                self.page = {**page, "fingerprint": "after"}
            def close(self):
                pass
        def result(operation, target=None, text=None):
            return {"choices": [{"message": {"content": json.dumps(dict(operation=operation, target=target, text=text))}}], "usage": {}}
        state = {"arm": "A", "purpose": None, "deadline": float("inf")}
        with patch.dict(os.environ, {"TEXT_MODEL": "test", "TEXT_MODEL_BASE_URL": "https://example.invalid/v1",
                                     "TEXT_MODEL_API_KEY": "test-only", "JEV_PLANNER": "0", "JEV_SAFE": "0"}), \
             patch.object(agent, "Browser", Browser), patch.object(agent, "choose", agent.choose), \
             patch.object(agent, "field_text", agent.field_text), patch.object(agent, "select_option", agent.select_option), \
             patch.object(model, "post_json", side_effect=[result("TYPE_TEXT", "1", "book"), result("DONE")]) as http:
            paired = control.install(agent, model, state, lambda *a, **kw: None)
            instance = paired("https://example.invalid", "Search for book")
            states = list(instance.run())
            self.assertEqual(states[-1]["status"], "done")
            self.assertEqual(instance.browser.typed, ["book"])
            self.assertEqual(http.call_count, 2)
            self.assertTrue(instance.state["text_calls"][0]["merged"])


if __name__ == "__main__":
    unittest.main()
