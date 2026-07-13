import importlib.util
from pathlib import Path


def _load_errors_module():
    path = Path(__file__).resolve().parents[1] / "llm_harness_sdk" / "errors.py"
    spec = importlib.util.spec_from_file_location("llm_harness_sdk_errors", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_error_json_maps_fields():
    errors = _load_errors_module()

    err = errors.HarnessError.from_error_json(
        '{"message":"bad request","code":400,"kind":"provider"}'
    )

    assert str(err) == "bad request"
    assert err.code == 400
    assert err.kind == "provider"
    assert err.payload["message"] == "bad request"


def test_error_json_falls_back_to_raw_text():
    errors = _load_errors_module()

    err = errors.HarnessError.from_error_json("plain failure")

    assert str(err) == "plain failure"
