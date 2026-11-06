import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

SCRIPTS = Path(__file__).resolve().parents[1] / "benchmarks/jev_browser"
sys.path.insert(0, str(SCRIPTS))
import kev_service as kev
import author_suite
import author_control
import audit_author_run


class KevServiceTests(unittest.TestCase):
    def test_invalid_arm_combinations_fail_before_side_effects(self):
        for args in ({}, {"paired": True}, {"paired": True, "controlled": True},
                     {"paired": True, "controlled": True, "text_backend": "deepseek-flash", "with_decider": True}):
            with self.subTest(args=args), self.assertRaises(ValueError):
                author_suite.run(Path("not-created"), proxy="http://127.0.0.1:7890", decision_backend="kev", **args)

    def test_failure_is_durable_and_scoped(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            ledger = kev.Ledger(directory / "kev-events.jsonl")
            ledger.scope = {"task": "a", "arm": "B", "repeat": 1}
            with self.assertRaises(ValueError):
                with ledger.measured("kev_predict", questions=2):
                    ledger.emit("kev_forward", device="cuda:0")
                    raise ValueError("Do not log error text or credentials")
            ledger.handle.close()
            records, counts = [], dict.fromkeys(kev.COUNTERS, 0)
            proc = kev.KevProcess(directory, 18000, lambda component, **r: records.append({"component": component, **r}), counts)
            proc.collect()
            proc.collect()
            self.assertEqual(counts, {"kev_rpc": 0, "kev_predict": 1, "kev_forward": 1})
            self.assertEqual(len(records), 3)
            self.assertEqual(records[-1]["status"], "error")
            self.assertEqual(records[-1]["task"], "a")
            self.assertNotIn("Do not log", json.dumps(records))

    def test_partial_ledger_line_is_not_consumed(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            path = directory / "kev-events.jsonl"
            row = json.dumps({"service_seq": 1, "component": "kev_rpc_start"}).encode()
            path.write_bytes(row)
            counts = dict.fromkeys(kev.COUNTERS, 0)
            proc = kev.KevProcess(directory, 18000, Mock(), counts)
            proc.collect()
            self.assertEqual(proc.offset, 0)
            path.write_bytes(row + b"\n")
            proc.collect()
            self.assertEqual(counts["kev_rpc"], 1)

    def test_scope_is_header_only_and_preserves_body(self):
        import httpx
        req = httpx.Request("POST", "http://127.0.0.1/v1/systemone", json={"state": "unchanged"})
        proc = kev.KevProcess(Path("unused"), 18000, Mock(), {})
        before = req.content
        proc.decorate(req, {"name": "case", "arm": "B", "repeat": 2, "deadline": 1})
        self.assertEqual(before, req.content)
        self.assertEqual(json.loads(req.headers["X-Kev-Scope"]), {"task": "case", "arm": "B", "repeat": 2})

    def test_reset_requires_idle_and_empty_cache(self):
        proc = kev.KevProcess(Path("unused"), 18000, Mock(), {})
        with patch.object(kev, "request", return_value={"idle": True, "cached_states": 0}):
            proc.reset()
        with patch.object(kev, "request", return_value={"idle": False, "cached_states": 2}):
            with self.assertRaises(kev.KevUnavailable):
                proc.reset()

    def test_unavailable_cannot_be_swallowed_as_a_task_exception(self):
        self.assertTrue(issubclass(kev.KevUnavailable, BaseException))
        self.assertFalse(issubclass(kev.KevUnavailable, Exception))

    def test_close_waits_for_service_and_collects_failures(self):
        proc = kev.KevProcess(Path("unused"), 18000, Mock(), {})
        proc.process = Mock()
        proc.process.poll.return_value = None
        proc.collect = Mock()
        with patch.object(kev, "request", return_value={"stopping": True}):
            proc.close()
        proc.process.wait.assert_called_once_with(timeout=10)
        proc.collect.assert_called_once()

    def test_audit_reconciles_independent_service_ledger(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            counters = {**dict.fromkeys(audit_author_run.COMPONENTS, 0), **dict.fromkeys(kev.COUNTERS, 0)}
            rows = [dict(task="sample", arm=arm, repeat=1, **{"pass": True},
                         attempt_wall_seconds=2., initial_observation_sha256="same", counts=dict(counters))
                    for arm in ("A", "B")]
            rows[1]["counts"].update(kev_rpc=1, kev_predict=1, kev_forward=1)
            events = [{"task": "sample", "arm": "B", "repeat": 1, "service_seq": i,
                       "component": component, "device": "cuda:0", "request_sha256": kev.preflight.digest({}),
                       "forward_count": 1 if component == "kev_predict" else 0}
                      for i, component in enumerate(("kev_rpc_start", "kev_predict_start", "kev_forward", "kev_predict", "kev_rpc"), 1)]
            client_event = {"task": "sample", "arm": "B", "repeat": 1, "component": "decision_http", "request_body": {},
                            "http_status": 200, "model": kev.MODEL}
            manifest = {"decision_backend": "kev", "kev": {"model": kev.MODEL}, "tasks": ["sample"],
                        "order": [{k: r[k] for k in ("task", "arm", "repeat")} for r in rows]}
            report = {"rows": rows, "completed": 2, "unstarted": 0, "counts_including_warmup": rows[1]["counts"],
                      "comparison": author_control.compare(rows)}
            for name, value in (("manifest", manifest), ("report", report)):
                (directory / (name + ".json")).write_text(json.dumps(value), encoding="utf-8")
            for name, value in (("attempts", rows), ("events", events + [client_event]), ("kev-events", events)):
                (directory / (name + ".jsonl")).write_text("".join(json.dumps(v) + "\n" for v in value), encoding="utf-8")
            self.assertTrue(audit_author_run.audit(directory)["counters_and_comparison_verified"])
            (directory / "kev-events.jsonl").write_text("", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "service ledger"):
                audit_author_run.audit(directory)

    def test_timings_do_not_double_count_nested_inference(self):
        events = [{"component": c, "elapsed_ms": ms} for c, ms in
                  (("qwen_http", 200), ("decision_http", 300), ("kev_rpc", 290),
                   ("kev_predict", 280), ("tool", 100), ("provider_backoff", 1000))]
        result = audit_author_run.timing_summary(events)
        self.assertEqual(result["model_call_seconds"], .5)
        self.assertEqual(result["tool_seconds"], .1)
        self.assertEqual(result["backoff_seconds"], 1)
        self.assertEqual(result["http_transport_error"], 1)


if __name__ == "__main__":
    unittest.main()
