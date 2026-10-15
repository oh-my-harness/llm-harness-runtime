"""Offline checks for credential handling and actual request accounting."""

import importlib.util
import io
import json
import os
from pathlib import Path
import unittest
from unittest.mock import patch
import urllib.error


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("preflight", ROOT / "scripts/benchmarks/preflight_laya_qwen.py")
PREFLIGHT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PREFLIGHT)
CONFIG = json.loads((ROOT / "configs/research/laya-qwen-preflight.json").read_text())["qwen"]


class QwenSmokeTests(unittest.TestCase):
    @patch.dict(os.environ, {}, clear=True)
    def test_missing_credential_does_not_make_request(self):
        def send(*args, **kwargs):
            self.fail("Must not send unauthenticated request")
        self.assertEqual(PREFLIGHT.qwen_smoke(CONFIG, send)["actual_calls"], 0)

    @patch.dict(os.environ, {"SILICONFLOW_API_KEY": "test-secret-only"})
    def test_success_uses_approved_model_without_logging_content_or_key(self):
        calls = []
        def send(request, timeout):
            calls.append(request)
            self.assertEqual(json.loads(request.data)["model"], "Qwen/Qwen2.5-72B-Instruct")
            self.assertEqual(request.get_header("Authorization"), "Bearer test-secret-only")
            return io.BytesIO(json.dumps({"choices": [{"message": {"content": "test-secret-only"}}]}).encode())
        result = PREFLIGHT.qwen_smoke(CONFIG, send)
        self.assertEqual((len(calls), result["actual_calls"], result["retries"]), (1, 1, 0))
        self.assertEqual(result["status"], "ok")
        self.assertIsNone(result["usage"]["prompt_tokens"])
        self.assertNotIn("test-secret-only", json.dumps(result))
        self.assertGreaterEqual(result["elapsed_ms"], 0)

    @patch.dict(os.environ, {"SILICONFLOW_API_KEY": "test-secret-only"})
    def test_failure_is_counted_without_body_or_exception_message(self):
        def send(*args, **kwargs):
            raise urllib.error.HTTPError(CONFIG["endpoint"], 401, "test-secret-only", {}, io.BytesIO(b"test-secret-only"))
        result = PREFLIGHT.qwen_smoke(CONFIG, send)
        self.assertEqual(result["actual_calls"], 1)
        self.assertEqual(result["http_status"], 401)
        self.assertIsNone(result["usage"])
        self.assertNotIn("test-secret-only", json.dumps(result))

    @patch.dict(os.environ, {"SILICONFLOW_API_KEY": "test-secret-only"})
    def test_rejects_changed_credential_destination(self):
        with self.assertRaises(ValueError):
            PREFLIGHT.qwen_smoke({**CONFIG, "endpoint": "https://example.org"})

    @patch.dict(os.environ, {"SILICONFLOW_API_KEY": "test-secret-only"})
    def test_empty_response_is_not_success(self):
        result = PREFLIGHT.qwen_smoke(CONFIG, lambda *a, **kw: io.BytesIO(b'{"choices":[{"message":{"content":""}}]}'))
        self.assertEqual(result["status"], "invalid_response")
        self.assertEqual(result["actual_calls"], 1)

    @patch.dict(os.environ, {"SILICONFLOW_API_KEY": "test-secret-only"})
    def test_timeout_is_counted_once_without_retry_or_exception_text(self):
        def send(*args, **kwargs):
            raise TimeoutError("test-secret-only")
        result = PREFLIGHT.qwen_smoke(CONFIG, send)
        self.assertEqual((result["actual_calls"], result["retries"]), (1, 0))
        self.assertEqual(result["status"], "error")
        self.assertNotIn("test-secret-only", json.dumps(result))

    def test_redirect_is_not_followed(self):
        self.assertIsNone(PREFLIGHT.NoRedirect().redirect_request(None, None, 307, "", {}, "https://example.org"))


if __name__ == "__main__":
    unittest.main()
