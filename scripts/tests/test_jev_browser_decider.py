"""Offline protocol checks; the CUDA parity preflight is separate from model evaluation."""

from pathlib import Path
import json
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "benchmarks/jev_browser"))
import author_control
import author_suite
import decider_adapter as adapter
import audit_decider_run


class DeciderAdapterTests(unittest.TestCase):
    def test_schedule_preserves_ab_relative_order_and_visits_all_arms(self):
        tasks = [(str(i),) for i in range(16)]
        for repeat in range(2):
            actual = adapter.schedule(tasks, repeat)
            self.assertEqual(len(actual), 48)
            self.assertEqual([x for x in actual if x[1] != "C"], author_control.schedule(tasks, repeat))
            for index, task in enumerate(tasks):
                arms = [arm for t, arm in actual if t == task]
                self.assertEqual(set(arms), {"A", "B", "C"})
                self.assertEqual(arms.index("C"), (index + repeat) % 3)

    def test_singleton_keeps_one_real_option_and_instruction(self):
        question = {"type": "choice", "instructions": {"goal": "Fill the query"}, "criteria": {"4": "Search box"}}
        rendered = adapter.render_choice(question, lambda _: self.fail("Width-one upstream renderer rejects it"))
        self.assertEqual(rendered["names"], ["4"])
        self.assertEqual(rendered["options"], ["4: Search box"])
        self.assertEqual(rendered["question"], '{"goal": "Fill the query"}')
        self.assertEqual(question["criteria"], {"4": "Search box"})

    def test_multiple_candidates_use_author_renderer_unchanged(self):
        question = {"type": "choice", "criteria": {"1": "a", "2": "b"}}
        received = []
        result = adapter.render_choice(question, lambda value: received.append(value) or "native")
        self.assertEqual(result, "native")
        self.assertIs(received[0], question)

    def test_answers_cannot_add_omit_or_invent_options(self):
        questions = {"op": {"criteria": {"A": "a", "B": "b"}}}
        valid = {"answers": {"op": {"choice": "A", "probabilities": {"A": .8, "B": .2}}}}
        self.assertIs(adapter.validate_answers(questions, valid), valid)
        invalid = [{"answers": {}}, {"answers": {"op": {"choice": "C", "probabilities": {"A": 1., "B": 0.}}}},
                   {"answers": {"op": {"choice": "A", "probabilities": {"A": 1.}}}},
                   {"answers": {"op": {"choice": "A", "probabilities": {"A": float("nan"), "B": .2}}}}]
        for value in invalid:
            with self.assertRaises(ValueError):
                adapter.validate_answers(questions, value)

    def test_third_arm_requires_original_controlled_flash_conditions(self):
        with self.assertRaisesRegex(ValueError, "Decider comparison"):
            author_suite.run(Path("unused"), with_decider=True)

    def test_compare_does_not_silently_drop_c(self):
        rows = [{"task": "x", "repeat": 1, "arm": arm, "pass": passed,
                 "attempt_wall_seconds": seconds, "counts": {"qwen_http": 1}}
                for arm, passed, seconds in (("A", True, 6), ("B", True, 4), ("C", False, 8))]
        result = adapter.compare(rows)
        self.assertEqual(set(result), {"A_vs_B", "A_vs_C", "B_vs_C"})
        self.assertEqual(result["A_vs_B"]["paired_speed_ratio_A_over_B"], 1.5)
        self.assertEqual(result["A_vs_C"]["quality_pairs"]["only_A"], 1)
        self.assertEqual(result["B_vs_C"]["groups"]["B"]["passed"], 0)

    def test_audit_reconciles_ledger_and_detects_unreported_third_arm_calls(self):
        counters = {key: 0 for key in ("qwen_http", "laya_rpc", "laya_predict", "laya_forward", "tools",
                    "decider_rpc", "decider_predict", "decider_forward", "qwen_input_tokens", "qwen_output_tokens")}
        rows = [{"task": "x", "arm": arm, "repeat": 1, "pass": True, "attempt_wall_seconds": 1.,
                 "initial_observation_sha256": "same", "counts": dict(counters)} for arm in ("C", "A", "B")]
        manifest = {"order": [{key: row[key] for key in ("task", "arm", "repeat")} for row in rows],
                    "tasks": ["x"], "repeats": 1, "decider": {"version": "decider-2b-v11"}}
        report = {"rows": rows, "completed": 3, "unstarted": 0, "counts_including_warmup": counters,
                  "comparison": adapter.compare(rows)}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, data in (("report.json", report), ("manifest.json", manifest)):
                (root / name).write_text(json.dumps(data), encoding="utf-8")
            (root / "attempts.jsonl").write_text("\n".join(map(json.dumps, rows)), encoding="utf-8")
            (root / "events.jsonl").write_text("", encoding="utf-8")
            checked = audit_decider_run.audit(root)
            self.assertEqual(checked["complete_repeats"], [1])
            self.assertEqual(checked["matching_initial_observations"], {"A_vs_B": 1, "A_vs_C": 1, "B_vs_C": 1})
            event = {"task": "x", "arm": "C", "repeat": 1, "component": "decider_forward", "device": "cuda:0"}
            (root / "events.jsonl").write_text(json.dumps(event), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "Counter mismatch"):
                audit_decider_run.audit(root)


if __name__ == "__main__":
    unittest.main()
