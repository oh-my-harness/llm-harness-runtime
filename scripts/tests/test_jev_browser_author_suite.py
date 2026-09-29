"""Offline checks for author-source provenance and telemetry, without model imports."""

import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "benchmarks/jev_browser/author_suite.py"
SPEC = importlib.util.spec_from_file_location("author_suite", SCRIPT)
suite = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(suite)


class AuthorSuiteTests(unittest.TestCase):
    def test_source_validation_rejects_changes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "source.py").write_bytes(b"reviewed")
            expected = {"source.py": hashlib.sha256(b"reviewed").hexdigest()}
            suite.verify_files(root, expected)
            (root / "source.py").write_bytes(b"changed")
            with self.assertRaisesRegex(ValueError, "Author file mismatch"):
                suite.verify_files(root, expected)

    def test_missing_source_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(FileNotFoundError):
                suite.verify_files(Path(directory), {"absent.py": "missing"})

    def test_known_usage_accumulates(self):
        counts = {"qwen_input_tokens": 3, "qwen_output_tokens": 2}
        suite.add_usage(counts, {"prompt_tokens": 5, "completion_tokens": 0})
        self.assertEqual(counts, {"qwen_input_tokens": 8, "qwen_output_tokens": 2})

    def test_unknown_usage_is_not_zero(self):
        counts = {"qwen_input_tokens": 3, "qwen_output_tokens": 2}
        suite.add_usage(counts, {"prompt_tokens": 5})
        self.assertEqual(counts, {"qwen_input_tokens": 8, "qwen_output_tokens": None})
        suite.add_usage(counts, {"prompt_tokens": 1, "completion_tokens": 4})
        self.assertIsNone(counts["qwen_output_tokens"])
        suite.add_usage(counts, None)
        self.assertIsNone(counts["qwen_input_tokens"])

    def test_invalid_usage_stays_unknown(self):
        for value in (True, "5", -1):
            counts = {"qwen_input_tokens": 0, "qwen_output_tokens": 0}
            suite.add_usage(counts, {"prompt_tokens": value})
            self.assertIsNone(counts["qwen_input_tokens"])

    def test_checkbox_string_false_does_not_pass(self):
        for value in (False, "false", None, ""):
            self.assertFalse(suite.first_checkbox_checked({"actions": [{"role": "checkbox", "checked": value}]}))
        for value in (True, "true"):
            self.assertTrue(suite.first_checkbox_checked({"actions": [{"role": "checkbox", "checked": value}]}))
        self.assertFalse(suite.first_checkbox_checked({"actions": []}))


if __name__ == "__main__":
    unittest.main()
