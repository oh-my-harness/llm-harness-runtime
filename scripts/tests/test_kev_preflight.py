import copy
import importlib.util
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "benchmarks/jev_browser/kev_preflight.py"
spec = importlib.util.spec_from_file_location("kev_preflight", SCRIPT)
kev = importlib.util.module_from_spec(spec)
spec.loader.exec_module(kev)


class KevPreflightTests(unittest.TestCase):
    def setUp(self):
        self.questions = {"target": {"type": "choice", "criteria": {"1": "First", "2": "Second"}}}
        self.result = {"answers": {"target": {"choice": "1", "probabilities": {"1": .6, "2": .4}}}}

    def test_valid_choice(self):
        self.assertIs(kev.validate(self.questions, self.result), self.result)

    def test_singleton(self):
        questions = {"q": {"type": "choice", "criteria": {"only": "Only candidate"}}}
        kev.validate(questions, {"answers": {"q": {"choice": "only", "probabilities": {"only": 1.}}}})

    def test_invalid_distributions_and_ids(self):
        invalid = [float("nan"), float("inf"), -.1, 1.1, True, "0.6"]
        for p in invalid:
            with self.subTest(p=p):
                result = copy.deepcopy(self.result)
                result["answers"]["target"]["probabilities"]["1"] = p
                with self.assertRaises(ValueError):
                    kev.validate(self.questions, result)
        for mutate in (
            lambda r: r["answers"].clear(),
            lambda r: r["answers"]["target"].update(choice="missing"),
            lambda r: r["answers"]["target"]["probabilities"].update(extra=0.),
            lambda r: r["answers"]["target"]["probabilities"].update({"1": .1}),
        ):
            result = copy.deepcopy(self.result)
            mutate(result)
            with self.assertRaises(ValueError):
                kev.validate(self.questions, result)

    def test_reports_flips_without_hiding_probability_difference(self):
        other = {"target": {"choice": "2", "probabilities": {"1": .4, "2": .6}}}
        result = kev.differences(self.result["answers"], other)
        self.assertEqual(result["choice_flips"], ["target"])
        self.assertAlmostEqual(result["max_probability_delta"], .2)

    def test_digest_tracks_candidate_content(self):
        # The corpus hash is canonical JSON; actual request insertion order is retained separately in encoding.
        original = copy.deepcopy(self.questions)
        self.assertEqual(kev.digest(original), kev.digest(self.questions))
        original["target"]["criteria"]["1"] = "Changed"
        self.assertNotEqual(kev.digest(original), kev.digest(self.questions))

    def test_hashes_snapshot_files(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "config.json"
            kev.save(path, {"base": "test"})
            self.assertEqual(kev.verify_snapshot(root), {"config.json": kev.file_sha(path)})
            with self.assertRaises(ValueError):
                kev.verify_snapshot(root, {"config.json": "wrong"})
            with self.assertRaises(ValueError):
                kev.verify_snapshot(root, {"missing": "wrong"})


if __name__ == "__main__":
    unittest.main()
