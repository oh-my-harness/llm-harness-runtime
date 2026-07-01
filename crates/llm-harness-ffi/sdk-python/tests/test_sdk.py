import asyncio
import json
import threading
import unittest
from concurrent.futures import ThreadPoolExecutor

from llm_harness_sdk import Harness, HarnessError, native_abi_version, native_version
from llm_harness_sdk.binding import CALLBACK_ERROR_CODE, ffi, lib


def invoke_callback(callback, args):
    args_json = ffi.new("char[]", json.dumps(args).encode("utf-8"))
    result = ffi.new("char**")
    rc = callback(args_json, ffi.NULL, result)
    if result[0] == ffi.NULL:
        return rc, None
    try:
        return rc, json.loads(ffi.string(result[0]).decode("utf-8"))
    finally:
        lib.ffi_harness_free_string(result[0])


class SdkTests(unittest.TestCase):
    def test_native_version_contract(self):
        self.assertEqual(native_abi_version(), 2)
        self.assertRegex(native_version(), r"^\d+\.\d+\.\d+")

    def test_invalid_config_reports_native_error(self):
        with self.assertRaises(HarnessError) as caught:
            Harness(model="gpt-4o-mini", api_key="test")

        self.assertEqual(getattr(caught.exception, "kind", None), "missing_provider")

    def test_register_tool_without_network_call(self):
        with Harness(provider="openai", model="gpt-4o-mini", api_key="test") as harness:

            @harness.tool(
                "echo",
                description="Echoes text",
                parameters_schema={
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                },
            )
            def echo(args, ctx):
                return {
                    "content": [{"type": "text", "text": args.get("text", "")}],
                    "details": {},
                    "terminate": False,
                }

            self.assertIsNotNone(echo)

    def test_close_is_idempotent_and_guards_closed_handle(self):
        harness = Harness(provider="openai", model="gpt-4o-mini", api_key="test")
        harness.close(timeout=0.1)
        harness.close(timeout=0.1)

        with self.assertRaises(HarnessError):
            harness.prompt("should fail")

    def test_aggregate_events_returns_final_response_shape(self):
        response = Harness.aggregate_events(
            [
                {"type": "text_delta", "text": "hel"},
                {
                    "type": "tool_start",
                    "tool_call_id": "call-1",
                    "tool_name": "echo",
                    "args": {"text": "x"},
                },
                {
                    "type": "tool_end",
                    "tool_call_id": "call-1",
                    "tool_name": "echo",
                    "result": {"content": [{"type": "text", "text": "x"}]},
                },
                {"type": "text_delta", "text": "lo"},
                {
                    "type": "message_end",
                    "message": {
                        "content": [{"type": "text", "text": "hello"}],
                        "usage": {"input_tokens": 1, "output_tokens": 2},
                    },
                },
                {"type": "settled"},
            ]
        )

        self.assertEqual(response["text"], "hello")
        self.assertEqual(response["usage"], {"input_tokens": 1, "output_tokens": 2})
        self.assertEqual(response["tool_calls"][0]["name"], "echo")
        self.assertEqual(response["tool_calls"][0]["result"]["content"][0]["text"], "x")

    def test_collect_until_settled_stops_on_terminal_events(self):
        for event_type in ("aborted", "error"):
            with self.subTest(event_type=event_type):
                harness = object.__new__(Harness)
                events = iter([{"type": event_type, "error": {"message": "boom"}}])
                harness.wait_event = lambda timeout=1.0: next(events)

                collected = harness.collect_until_settled(timeout=0.1)

                self.assertEqual(collected[0]["type"], event_type)

    def test_async_callbacks_use_per_harness_runner_and_close_cleanly(self):
        async def thread_marker(args):
            await asyncio.sleep(0)
            return {
                "content": [{"type": "text", "text": args["text"]}],
                "details": {"thread_id": threading.get_ident()},
                "terminate": False,
            }

        first = Harness(provider="openai", model="gpt-4o-mini", api_key="test")
        second = Harness(provider="openai", model="gpt-4o-mini", api_key="test")

        first_callback = first._build_callback(thread_marker)
        second_callback = second._build_callback(thread_marker)

        first_rc, first_result = invoke_callback(first_callback, {"text": "one"})
        second_rc, second_result = invoke_callback(second_callback, {"text": "two"})

        self.assertEqual(first_rc, 0)
        self.assertEqual(second_rc, 0)
        self.assertEqual(first_result["content"][0]["text"], "one")
        self.assertEqual(second_result["content"][0]["text"], "two")
        self.assertNotEqual(
            first_result["details"]["thread_id"],
            second_result["details"]["thread_id"],
        )
        self.assertTrue(first._async_runner.is_alive())
        self.assertTrue(second._async_runner.is_alive())

        first_runner = first._async_runner
        second_runner = second._async_runner
        first.close(timeout=0.1)
        second.close(timeout=0.1)

        self.assertFalse(first_runner.is_alive())
        self.assertFalse(second_runner.is_alive())

    def test_async_callback_error_returns_tool_error_payload(self):
        async def failing_tool(_args):
            await asyncio.sleep(0)
            raise RuntimeError("async failure")

        with Harness(provider="openai", model="gpt-4o-mini", api_key="test") as harness:
            callback = harness._build_callback(failing_tool)
            rc, result = invoke_callback(callback, {})

        self.assertEqual(rc, CALLBACK_ERROR_CODE)
        self.assertIn("async failure", result["error"])

    def test_multiple_harnesses_register_and_close_concurrently(self):
        def worker(index):
            with Harness(provider="openai", model="gpt-4o-mini", api_key="test") as harness:

                @harness.tool(f"echo_{index}")
                def echo(args):
                    return {
                        "content": [{"type": "text", "text": args.get("text", "")}],
                        "details": {"index": index},
                        "terminate": False,
                    }

                return echo.__name__

        with ThreadPoolExecutor(max_workers=5) as executor:
            results = list(executor.map(worker, range(10)))

        self.assertEqual(results, ["echo"] * 10)

    def test_abort_then_close_is_idempotent(self):
        harness = Harness(provider="openai", model="gpt-4o-mini", api_key="test")
        harness.abort()
        harness.close(timeout=0.1)
        harness.close(timeout=0.1)

        with self.assertRaises(HarnessError):
            harness.abort()

    def test_repeated_register_close_cycles_without_network_call(self):
        for index in range(25):
            with Harness(provider="openai", model="gpt-4o-mini", api_key="test") as harness:

                @harness.tool(f"cycle_{index}")
                def cycle_tool(args):
                    return {
                        "content": [{"type": "text", "text": args.get("text", "")}],
                        "details": {"index": index},
                        "terminate": False,
                    }

                self.assertEqual(cycle_tool.__name__, "cycle_tool")


if __name__ == "__main__":
    unittest.main(verbosity=2)
