"""Offline contract tests only. Scripted doubles are never experiment arms."""

from copy import deepcopy
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "benchmarks"))

from browser_policy import Policy, parse_baseline, parse_laya
from experiment_metrics import Trace, summarize
from isolated_browser import validate_acceptance
from jev_reference.browser import StalePage
from jev_reference.protocol import build_request
from pilot_models import InputBudgetError, Laya
from run_laya_browser import run_attempt, validate_tasks


PAGE = {"url": "http://127.0.0.1:1234/", "title": "Contract fixture", "text": "Public observation",
        "fingerprint": "before", "actions": [
            {"id": "e1", "kind": "fill", "node": 1, "role": "textbox", "label": "Name", "value": ""},
            {"id": "e2", "kind": "click", "node": 1, "role": "textbox", "label": "Open Name", "value": ""},
            {"id": "e3", "kind": "click", "node": 2, "role": "button", "label": "Save", "value": ""},
            {"id": "e4", "kind": "select", "node": 3, "role": "combobox", "label": "Tier → Gold",
             "value": "gold", "current_value": "Silver"},
            {"id": "wait", "kind": "wait", "label": "Wait for page update"},
        ]}
TASK = {"id": "contract-only", "goal": "Enter Demo and save.", "url": PAGE["url"],
        "allowed_origins": ["http://127.0.0.1:1234"],
        "reset_policy": "fresh_browser_context_client_state_only",
        "acceptance": {"visible_text": ["INDEPENDENT_CHECKER_SENTINEL"]}}
CONFIG = {"task_timeout_seconds": 10, "max_decisions": 10, "max_actions": 8}


def answer(criteria, choice):
    return {"choice": choice, "confidence": 1.0,
            "probabilities": {key: float(key == choice) for key in criteria}}


class ScriptedQwen:
    key = "private-test-sentinel"

    def __init__(self, responses):
        self.responses, self.inputs = iter(responses), []

    def call(self, system, state, event, remaining):
        self.inputs.append((system, state))
        event.update(actual_calls=1, retry_index=0)
        value = next(self.responses)
        if isinstance(value, Exception):
            raise value
        return json.dumps(value)


class ScriptedLaya:
    def __init__(self, decisions):
        self.decisions, self.inputs = iter(decisions), []

    def predict(self, state, questions, event):
        self.inputs.append((state, questions))
        event.update(actual_calls=1, forward_attempts=1, decision_questions=len(questions), retry_index=0)
        operation, target = next(self.decisions)
        answers = {"operation": answer(questions["operation"]["criteria"], operation)}
        if target is not None:
            head = operation.lower() + "_target"
            answers[head] = answer(questions[head]["criteria"], target)
        return answers


class FakeBrowser:
    def __init__(self, *_args):
        self.page = Mock()
        self.chrome = Mock(version="offline-test-double")
        self.session = Mock(calls={}, failures=0, elapsed_ms=0.0)
        self.blocked_requests, self.executed, self.observations = 0, [], 0

    def observe(self, **_kwargs):
        self.observations += 1
        page = deepcopy(PAGE)
        page["fingerprint"] = str(len(self.executed))
        return page

    def fresh(self, *_args):
        return True

    def act(self, action, page, text=None):
        self.executed.append((action, text))
        return {"executed": action["id"]}

    def close(self):
        pass


class BrowserPolicyTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.trace = Trace(self.root / "trace.jsonl", ScriptedQwen.key)

    def tearDown(self):
        self.trace.close()
        self.temp.cleanup()

    def test_reference_operation_heads_share_observed_element_index(self):
        request = build_request(PAGE, TASK["goal"], [])
        heads = request["questions"]
        self.assertEqual(set(heads), {"operation", "click_target", "type_text_target", "select_target"})
        self.assertIn("1", heads["type_text_target"]["criteria"])
        self.assertIn("1", heads["click_target"]["criteria"])
        self.assertIn("3:1", heads["select_target"]["criteria"])
        self.assertNotIn("TYPESAFE", json.dumps(request))

    def test_only_selected_target_head_can_execute(self):
        request = build_request(PAGE, TASK["goal"], [])
        answers = {"operation": answer(request["questions"]["operation"]["criteria"], "CLICK"),
                   "click_target": answer(request["questions"]["click_target"]["criteria"], "2"),
                   "type_text_target": {"choice": "invented"}, "select_target": {"code": "bad"}}
        decision, action = parse_laya(answers, request, PAGE)
        self.assertEqual((decision["operation"], action["id"]), ("CLICK", "e3"))

    def test_unobserved_target_and_injected_execution_fields_are_rejected(self):
        for value in ({"operation": "CLICK", "target": "999"},
                      {"operation": "CLICK", "target": "3:1"},
                      {"operation": "DONE", "target": "2"},
                      {"operation": "CLICK", "target": "2", "selector": "#save"},
                      {"operation": "CLICK", "target": "2", "code": "arbitrary"},
                      {"operation": "CLICK", "target": "2", "text": "unexpected"}):
            with self.subTest(value=value), self.assertRaises(ValueError):
                parse_baseline(json.dumps(value), PAGE)

    def test_click_select_wait_done_do_not_call_text_model(self):
        qwen = ScriptedQwen([])
        laya = ScriptedLaya([("CLICK", "2"), ("SELECT", "3:1"), ("WAIT", None), ("DONE", None)])
        policy = Policy("qwen_laya", qwen, laya, self.trace)
        for step in range(4):
            decision, action = policy.decide(PAGE, TASK["goal"], [], step, lambda: 10)
            self.assertNotEqual(decision["operation"], "TYPE_TEXT")
        self.assertEqual(len(qwen.inputs), 0)
        self.assertEqual(len(laya.inputs), 4)

    def test_laya_selects_field_then_qwen_only_generates_its_text(self):
        qwen, laya = ScriptedQwen([{"text": "Demo"}]), ScriptedLaya([("TYPE_TEXT", "1")])
        policy = Policy("qwen_laya", qwen, laya, self.trace)
        decision, action = policy.decide(PAGE, TASK["goal"], [], 0, lambda: 10)
        value = policy.text(decision, action, PAGE, TASK["goal"], [], 0, lambda: 10)
        self.assertEqual(value, "Demo")
        context = json.loads(qwen.inputs[0][1])
        self.assertEqual(context["field"]["label"], "Name")
        self.assertNotIn("questions", context)
        self.assertEqual([e["purpose"] for e in self.trace.events if e["component"] == "qwen"], ["generation"])

    def test_qwen_baseline_combines_type_decision_and_text_in_one_call(self):
        qwen = ScriptedQwen([{"operation": "TYPE_TEXT", "target": "1", "text": "Demo"}])
        policy = Policy("qwen", qwen, None, self.trace)
        decision, action = policy.decide(PAGE, TASK["goal"], [], 0, lambda: 10)
        self.assertEqual(policy.text(decision, action, PAGE, TASK["goal"], [], 0, lambda: 10), "Demo")
        self.assertEqual(len(qwen.inputs), 1)
        self.assertEqual(self.trace.events[-1]["purpose"], "mixed")

    def test_text_helper_cannot_change_action(self):
        qwen = ScriptedQwen([{"text": "Demo", "operation": "CLICK"}])
        policy = Policy("qwen_laya", qwen, None, self.trace)
        with self.assertRaises(ValueError):
            policy.text({"operation": "TYPE_TEXT"}, PAGE["actions"][0], PAGE, TASK["goal"], [], 0, lambda: 10)

    def test_stale_text_cache_requires_identical_entire_input(self):
        qwen = ScriptedQwen([{"text": "Demo"}, {"text": "New"}, {"text": "Again"}])
        policy = Policy("qwen_laya", qwen, None, self.trace)
        args = ({"operation": "TYPE_TEXT"}, PAGE["actions"][0], PAGE, TASK["goal"], [], 0, lambda: 10)
        self.assertEqual(policy.text(*args), policy.text(*args))
        self.assertEqual(len(qwen.inputs), 1)
        changed = deepcopy(PAGE)
        changed["text"] += " changed"
        self.assertEqual(policy.text(*args[:2], changed, *args[3:]), "New")
        policy.executed()
        self.assertEqual(policy.text(*args), "Again")

    def attempt(self, qwen, laya, arm="qwen_laya", accepted=False, browser=None):
        browser = browser or FakeBrowser()
        with patch("run_laya_browser.IsolatedBrowser", return_value=browser), patch(
                "run_laya_browser.evaluate", return_value={"success": accepted, "passed": int(accepted), "total": 1}):
            result = run_attempt(TASK, 0, arm, CONFIG, self.root, qwen, laya)
        return result, browser

    def test_done_is_not_independent_success_and_checker_stays_private(self):
        qwen, laya = ScriptedQwen([]), ScriptedLaya([("DONE", None)])
        result, _ = self.attempt(qwen, laya)
        self.assertFalse(result["success"])
        self.assertEqual(result["termination"], "done")
        self.assertEqual(result["calls"], {"qwen": 0, "laya": 1})
        self.assertNotIn("INDEPENDENT_CHECKER_SENTINEL", json.dumps(laya.inputs))

    def test_complete_hybrid_loop_counts_both_models(self):
        result, browser = self.attempt(ScriptedQwen([{"text": "Demo"}]),
                                       ScriptedLaya([("TYPE_TEXT", "1"), ("DONE", None)]), accepted=True)
        self.assertTrue(result["success"])
        self.assertEqual(result["calls"], {"qwen": 1, "laya": 2})
        self.assertEqual(result["qwen_by_purpose"], {"generation": 1})
        self.assertEqual(browser.executed[0][1], "Demo")

    def test_observation_stale_after_mutation_does_not_reexecute(self):
        browser = FakeBrowser()
        original = browser.observe
        failed = False

        def observe(**kwargs):
            nonlocal failed
            if browser.executed and not failed:
                failed = True
                raise StalePage("navigation during read")
            return original(**kwargs)

        browser.observe = observe
        result, _ = self.attempt(ScriptedQwen([]), ScriptedLaya([("CLICK", "2"), ("DONE", None)]),
                                 accepted=True, browser=browser)
        self.assertTrue(result["success"])
        self.assertEqual(len(browser.executed), 1)
        self.assertEqual(result["stale_decisions"], 1)

    def test_model_error_is_counted_and_included_as_failure(self):
        result, _ = self.attempt(ScriptedQwen([TimeoutError("private-test-sentinel")]), None, arm="qwen")
        self.assertFalse(result["success"])
        self.assertEqual(result["calls"]["qwen"], 1)
        self.assertEqual(result["error_type"], "TimeoutError")
        self.assertIsNotNone(result["elapsed_ms"])
        self.assertNotIn(ScriptedQwen.key, (self.root / (result["id"] + ".jsonl")).read_text())
        summary = summarize([result])
        self.assertIsNone(summary["paired_speedup"])
        self.assertEqual(summary["pair_outcomes"]["incomplete"], 1)

    def test_failed_pairs_cannot_supply_a_speedup(self):
        row, _ = self.attempt(ScriptedQwen([]), ScriptedLaya([("DONE", None)]))
        other = deepcopy(row)
        other.update(arm="qwen", elapsed_ms=row["elapsed_ms"] * 100)
        report = summarize([row, other])
        self.assertIsNone(report["paired_speedup"])
        self.assertEqual(report["pair_outcomes"]["both_fail"], 1)
        self.assertEqual(report["arms"]["qwen_laya"]["pass_rate"], 0)

    def test_summary_exposes_success_coverage_failures_and_real_call_counts(self):
        row, _ = self.attempt(ScriptedQwen([]), ScriptedLaya([("DONE", None)]), accepted=True)
        row.update(elapsed_ms=10.0, whole_attempt_ms=15.0)
        baseline = deepcopy(row)
        baseline.update(arm="qwen", elapsed_ms=20.0, calls={"qwen": 2, "laya": 0})
        failed = deepcopy(row)
        failed.update(task_id="different", success=False, elapsed_ms=1.0)
        failed_a = deepcopy(failed)
        failed_a.update(arm="qwen", success=True, elapsed_ms=10.0, calls={"qwen": 3, "laya": 0})
        report = summarize([baseline, row, failed_a, failed])
        self.assertEqual(report["paired_speedup"], 2.0)
        self.assertEqual(report["pass_rate_delta_pp"], -50.0)
        self.assertEqual(report["successful_pair_coverage"], 0.5)
        self.assertEqual(report["pair_outcomes"]["qwen_only"], 1)
        self.assertEqual(report["arms"]["qwen"]["qwen_calls"], 5)
        self.assertAlmostEqual(report["arms"]["qwen"]["all_latency"]["p95_ms"], 19.5)
        self.assertEqual(report["arms"]["qwen_laya"]["amortized_task_ms_per_success"], 11.0)

    def test_setup_failure_is_preserved_in_quality_denominator(self):
        with patch("run_laya_browser.IsolatedBrowser", side_effect=RuntimeError("setup failed")):
            result = run_attempt(TASK, 0, "qwen", CONFIG, self.root, ScriptedQwen([]), None)
        report = summarize([result])
        self.assertIsNone(result["elapsed_ms"])
        self.assertEqual(result["calls"]["qwen"], 0)
        self.assertEqual(report["arms"]["qwen"]["setup_failures"], 1)
        self.assertEqual(report["arms"]["qwen"]["pass_rate"], 0.0)

    def test_pending_configuration_cannot_load_models(self):
        from experiment_metrics import dump
        from run_laya_browser import main
        tasks_path = self.root / "tasks.json"
        dump(tasks_path, [TASK])
        configuration = Path(__file__).resolve().parents[2] / "configs/research/laya-browser-development.json"
        argv = ["runner", "--config", str(configuration), "--tasks", str(tasks_path),
                "--output", str(self.root / "no-run")]
        with patch.object(sys, "argv", argv), patch("run_laya_browser.Qwen") as qwen, patch(
                "run_laya_browser.Laya") as laya, patch("sys.stderr"), self.assertRaises(SystemExit) as caught:
            main()
        self.assertEqual(caught.exception.code, 2)
        qwen.assert_not_called()
        laya.assert_not_called()
        self.assertFalse((self.root / "no-run").exists())

    def test_no_empty_checker_or_path_traversal_task_id(self):
        for spec in ({}, {"visible_text": []}, {"anything": True}):
            with self.assertRaises(ValueError):
                validate_acceptance(spec)
        task = deepcopy(TASK)
        task["id"] = "../outside"
        with self.assertRaises(ValueError):
            validate_tasks([task])

    def test_laya_budget_rejection_happens_before_inference(self):
        model = Laya.__new__(Laya)
        model.agent, model.config = Mock(), {"max_len": 512, "head_max_len": 192}
        audit = {"operation": {"state_tokens_dropped": 0, "head_tokens_dropped": 1,
                                "option_tokens_dropped_at_48": 0, "markers": 2,
                                "options": 2, "options_distinct": 2}}
        event = {}
        with patch("pilot_models.audit_laya_input", return_value=audit), self.assertRaises(InputBudgetError):
            model.predict("state", {"operation": {}}, event)
        model.agent.predict.assert_not_called()
        self.assertEqual(event["actual_calls"], 0)

    def test_laya_sends_multiple_heads_in_one_predict_and_counts_forwards(self):
        model = Laya.__new__(Laya)
        model.agent, model.torch = Mock(), Mock()
        model.config = {"max_len": 512, "head_max_len": 192}
        model.forwards = []
        model.agent.cpu_fallback_count = 0
        expected = {"operation": {}, "click_target": {}}

        def predict(state, questions, **kwargs):
            self.assertIs(questions, expected)
            model.forwards.extend(["cuda:0", "cuda:0"])
            return {"answers": {"operation": {"choice": "CLICK"}}}

        model.agent.predict.side_effect = predict
        with patch("pilot_models.audit_laya_input", return_value={}):
            event = {}
            model.predict("state", expected, event)
        model.agent.predict.assert_called_once()
        self.assertEqual((event["actual_calls"], event["decision_questions"], event["forward_attempts"]), (1, 2, 2))


if __name__ == "__main__":
    unittest.main()
