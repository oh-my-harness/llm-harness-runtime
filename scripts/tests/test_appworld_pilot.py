"""Offline protocol and accounting tests. Fixtures are synthetic, not benchmark data."""

import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch


sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "benchmarks"))
from appworld_agent import ActionMenu, parse_baseline, parse_code, state_text
from appworld_sandbox import command
from run_laya_appworld import Trace, run_attempt, summarize


METADATA = {
    "instruction": "Synthetic task, no private answers.", "supervisor": {"name": "Example"},
    "app_descriptions": {"example": "Synthetic API app"},
    "api_docs": {"example": {"method_" + str(i): {"description": "Public method"} for i in range(30)}},
    "ground_truth": "HIDDEN_SENTINEL",
}


class ProtocolTests(unittest.TestCase):
    def test_hidden_fields_do_not_enter_state(self):
        state = state_text(METADATA, [], ActionMenu(METADATA))
        self.assertNotIn("HIDDEN_SENTINEL", state)
        self.assertNotIn("ground_truth", state)

    def test_baseline_can_decide_and_generate_in_one_response(self):
        result, candidate = parse_baseline(
            '{"action":"generate","code":"print(1)"}', ActionMenu(METADATA)
        )
        self.assertEqual(candidate["kind"], "generate")
        self.assertEqual(result["code"], "print(1)")

    def test_no_free_code_generation_for_missing_parameters(self):
        with self.assertRaises(ValueError):
            parse_baseline('{"action":"generate"}', ActionMenu(METADATA))
        with self.assertRaises(ValueError):
            parse_code('{"code":""}')

    def test_unknown_choice_is_not_replaced_by_a_correct_action(self):
        with self.assertRaises(ValueError):
            ActionMenu(METADATA).selected("unknown")

    def test_candidate_pages_cover_all_public_api_names(self):
        menu = ActionMenu(METADATA)
        menu.navigate(menu.selected("app:example"))
        names = []
        while True:
            choices = menu.candidates()
            self.assertLessEqual(len(choices), 17)
            names.extend(c["api"] for c in choices if c["kind"] == "docs_api")
            if not any(c["id"] == "next" for c in choices):
                break
            menu.navigate(menu.selected("next"))
        self.assertEqual(sorted(names), sorted(METADATA["api_docs"]["example"]))
        menu.navigate(menu.selected("apps"))
        self.assertIsNone(menu.app)

    def test_sandbox_has_no_whole_host_mount_and_clears_environment(self):
        argv = command(Path("C:/example/data"), Path("C:/example/output"),
                       Path("C:/example/worker.py"), "abc_1", "test", "Ubuntu", "/opt/venv")
        self.assertIn("--unshare-all", argv)
        self.assertIn("--clearenv", argv)
        mounts = [argv[i + 1:i + 3] for i, v in enumerate(argv) if v in ("--bind", "--ro-bind")]
        self.assertNotIn(["/", "/"], mounts)
        self.assertEqual(sum(argv[i] == "--bind" for i in range(len(argv))), 1)
        self.assertNotIn("SILICONFLOW_API_KEY", " ".join(argv))

    def test_trace_counts_failed_attempt_and_scrubs_secrets(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "events.jsonl"
            trace = Trace(path, "fake-secret")
            with self.assertRaises(TimeoutError):
                with trace.event("qwen", 0, "mixed", actual_calls=1) as event:
                    event["response"] = "fake-secret"
                    raise TimeoutError("fake-secret")
            trace.close()
            lines = path.read_text().splitlines()
            self.assertEqual(len(lines), 2)
            self.assertNotIn("fake-secret", "\n".join(lines))
            end = json.loads(lines[-1])
            self.assertEqual(end["status"], "error")
            self.assertEqual(end["actual_calls"], 1)
            self.assertGreaterEqual(end["elapsed_ms"], 0)

    def test_early_failure_is_not_reported_as_effective_speedup(self):
        a = {"task_id": "a", "repetition": 0, "arm": "qwen", "success": True,
             "elapsed_ms": 100, "calls": {"qwen": 3, "laya": 0}, "tool_calls": 3}
        b = {**a, "arm": "qwen_laya", "success": False, "elapsed_ms": 1,
             "calls": {"qwen": 0, "laya": 1}}
        report = summarize([a, b])
        self.assertEqual(report["arms"]["qwen_laya"]["attempts"], 1)
        self.assertEqual(report["arms"]["qwen_laya"]["passed"], 0)
        self.assertIsNone(report["paired_speedup"])
        self.assertEqual(report["both_successful_pairs"], 0)

    def test_speedup_uses_both_successful_pairs_only(self):
        base = {"task_id": "a", "repetition": 0, "success": True,
                "calls": {"qwen": 1, "laya": 0}, "tool_calls": 1}
        result = summarize([{**base, "arm": "qwen", "elapsed_ms": 100},
                            {**base, "arm": "qwen_laya", "elapsed_ms": 50}])
        self.assertEqual(result["paired_speedup"], 2)


class FakeWorld:
    version = "test-version"
    metadata = METADATA

    def __init__(self, *args, **kwargs):
        self.completed = False

    def request(self, operation, **kwargs):
        if operation == "execute":
            self.completed = kwargs["code"] == "print(1)"
            return {"output": "ok", "task_completed": True, "execution_failed": False}
        if operation == "evaluate":
            return {"success": self.completed, "tests_passed": int(self.completed), "tests_total": 1}
        return {}

    def close(self):
        pass


class FakeQwen:
    key = "unit-test-credential"

    def call(self, system, state, event, remaining):
        event["actual_calls"] = 1
        if "HIDDEN_SENTINEL" in state:
            raise AssertionError("Hidden data leaked")
        return '{"action":"generate","code":"print(1)"}'


class FakeLaya:
    def choose(self, state, candidates, event):
        event.update(actual_calls=1, forward_attempts=1)
        return "generate"


class AttemptTests(unittest.TestCase):
    def run_sample(self, arm, model=None, decision=None):
        config = {"benchmark_version": "test-version", "api_page_size": 12,
                  "task_timeout_seconds": 1, "tool_timeout_seconds": 1, "max_steps": 2}
        with tempfile.TemporaryDirectory() as directory:
            with patch("run_laya_appworld.Sandbox", FakeWorld):
                return run_attempt("abc_1", 0, arm, config, Path(directory), Path(directory),
                                   model or FakeQwen(), decision or FakeLaya())

    def test_baseline_counts_single_mixed_request(self):
        result = self.run_sample("qwen")
        self.assertTrue(result["success"])
        self.assertEqual(result["calls"], {"qwen": 1, "laya": 0})
        self.assertEqual(result["qwen_by_purpose"], {"mixed": 1})
        self.assertEqual(result["tool_calls"], 1)

    def test_hybrid_counts_generation_and_real_forward_separately(self):
        result = self.run_sample("qwen_laya")
        self.assertTrue(result["success"])
        self.assertEqual(result["calls"], {"qwen": 1, "laya": 1})
        self.assertEqual(result["qwen_by_purpose"], {"generation": 1})
        self.assertEqual(result["laya_forward_attempts"], 1)

    def test_premature_completion_does_not_override_independent_failure(self):
        class Premature:
            def choose(self, state, candidates, event):
                event.update(actual_calls=1, forward_attempts=1)
                return "finish"
        result = self.run_sample("qwen_laya", decision=Premature())
        self.assertFalse(result["success"])
        self.assertEqual(result["calls"]["qwen"], 0)
        self.assertEqual(result["termination"], "completed_signal")

    def test_provider_failure_is_counted_and_still_evaluated(self):
        class FailingQwen(FakeQwen):
            def call(self, system, state, event, remaining):
                event["actual_calls"] = 1
                raise TimeoutError("unit-test-credential")
        result = self.run_sample("qwen", model=FailingQwen())
        self.assertFalse(result["success"])
        self.assertIsNotNone(result["evaluation"])
        self.assertEqual(result["calls"]["qwen"], 1)
        self.assertEqual(result["termination"], "agent_error")

    def test_cleanup_error_preserves_attempt_result(self):
        with patch.object(FakeWorld, "close", side_effect=RuntimeError("close failure")):
            result = self.run_sample("qwen")
        self.assertEqual(result["cleanup_error_type"], "RuntimeError")
        self.assertEqual(result["calls"]["qwen"], 1)


if __name__ == "__main__":
    unittest.main()
