import os
import unittest
from concurrent.futures import ThreadPoolExecutor, as_completed

from llm_harness_sdk import Harness


def _env(name, fallback=None):
    return os.environ.get(name) or (os.environ.get(fallback) if fallback else None)


def _api_key_env():
    for name in (
        "LLM_HARNESS_STRESS_API_KEY",
        "LLM_HARNESS_E2E_API_KEY",
        "LATTICE_API_KEY",
    ):
        if os.environ.get(name):
            return name
    return "LATTICE_API_KEY"


@unittest.skipUnless(
    os.environ.get("LLM_HARNESS_STRESS") == "1",
    "set LLM_HARNESS_STRESS=1 to run real provider stress tests",
)
class ProviderStressTests(unittest.TestCase):
    def _config(self):
        api_key_env = _api_key_env()
        required = {
            api_key_env: os.environ.get(api_key_env),
            "model": _env("LLM_HARNESS_STRESS_MODEL", "LATTICE_MODEL"),
            "base_url": _env("LLM_HARNESS_STRESS_BASE_URL", "LATTICE_API_BASE"),
        }
        missing = [name for name, value in required.items() if not value]
        if missing:
            self.skipTest(f"missing stress environment variables: {', '.join(missing)}")

        return {
            "provider": os.environ.get(
                "LLM_HARNESS_STRESS_PROVIDER", "openai-compatible"
            ),
            "model": required["model"],
            "api_key_env": api_key_env,
            "base_url": required["base_url"],
            "max_tokens": int(os.environ.get("LLM_HARNESS_STRESS_MAX_TOKENS", "32")),
        }

    def _collect_response(self, harness, label="prompt"):
        response = harness.get_final_response(
            timeout=float(os.environ.get("LLM_HARNESS_STRESS_TIMEOUT", "180"))
        )
        if response["errors"]:
            self.fail(f"{label}: provider returned errors: {response['errors']!r}")
        self.assertGreater(len(response["text"].strip()), 0, f"{label}: empty response")
        return response

    def _run_prompt(self, prompt):
        with Harness(**self._config()) as harness:
            harness.prompt(prompt)
            return self._collect_response(harness, prompt)

    def test_repeated_prompts_reach_settled(self):
        prompt_count = int(os.environ.get("LLM_HARNESS_STRESS_PROMPTS", "5"))
        with Harness(**self._config()) as harness:
            for index in range(prompt_count):
                harness.prompt(f"Reply with exactly one short sentence. Run {index}.")
                self._collect_response(harness, f"repeated prompt {index}")

    def test_concurrent_harnesses_reach_settled(self):
        concurrency = int(os.environ.get("LLM_HARNESS_STRESS_CONCURRENCY", "2"))
        with ThreadPoolExecutor(max_workers=concurrency) as executor:
            futures = [
                executor.submit(
                    self._run_prompt,
                    f"Reply with one short sentence for concurrent run {index}.",
                )
                for index in range(concurrency)
            ]
            for future in as_completed(futures):
                future.result()

    def test_close_during_active_prompt_does_not_crash(self):
        harness = Harness(**self._config())
        try:
            harness.prompt("Write a short sentence, then stop.")
            harness.close(
                timeout=float(os.environ.get("LLM_HARNESS_STRESS_TIMEOUT", "180"))
            )
        finally:
            try:
                harness.close(timeout=1)
            except Exception:
                pass


if __name__ == "__main__":
    unittest.main(verbosity=2)
