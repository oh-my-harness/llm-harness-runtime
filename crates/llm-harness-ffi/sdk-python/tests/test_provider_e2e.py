import os
import unittest
from collections import Counter

from llm_harness_sdk import Harness


def _env(name, fallback=None):
    return os.environ.get(name) or (os.environ.get(fallback) if fallback else None)


@unittest.skipUnless(
    os.environ.get("LLM_HARNESS_E2E") == "1",
    "set LLM_HARNESS_E2E=1 to run real provider E2E tests",
)
class ProviderE2ETests(unittest.TestCase):
    def _config(self):
        api_key_env = (
            "LLM_HARNESS_E2E_API_KEY"
            if os.environ.get("LLM_HARNESS_E2E_API_KEY")
            else "LATTICE_API_KEY"
        )
        required = {
            api_key_env: os.environ.get(api_key_env),
            "model": _env("LLM_HARNESS_E2E_MODEL", "LATTICE_MODEL"),
            "base_url": _env("LLM_HARNESS_E2E_BASE_URL", "LATTICE_API_BASE"),
        }
        missing = [name for name, value in required.items() if not value]
        if missing:
            self.skipTest(f"missing E2E environment variables: {', '.join(missing)}")

        return {
            "provider": os.environ.get("LLM_HARNESS_E2E_PROVIDER", "openai-compatible"),
            "model": required["model"],
            "api_key_env": api_key_env,
            "base_url": required["base_url"],
            "max_tokens": int(os.environ.get("LLM_HARNESS_E2E_MAX_TOKENS", "32")),
        }

    def test_openai_compatible_prompt_reaches_settled(self):
        prompt = os.environ.get(
            "LLM_HARNESS_E2E_PROMPT",
            "Reply with one short English sentence.",
        )

        with Harness(**self._config()) as harness:
            harness.prompt(prompt)
            events = harness.collect_until_settled(
                timeout=float(os.environ.get("LLM_HARNESS_E2E_TIMEOUT", "120"))
            )

        counts = Counter(event.get("type") for event in events)
        errors = [event for event in events if event.get("type") == "error"]
        text = "".join(
            event.get("text", "")
            for event in events
            if event.get("type") == "text_delta"
        )

        self.assertEqual(errors, [])
        self.assertEqual(counts["settled"], 1)
        self.assertEqual(counts["message_end"], 1)
        self.assertGreater(len(text.strip()), 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
