import json
import llm_harness_py


def test_roundtrip_primitives():
    assert llm_harness_py.to_json(None) == "null"
    assert llm_harness_py.to_json(True) == "true"
    assert llm_harness_py.to_json(42) == "42"
    assert llm_harness_py.to_json(3.14) == "3.14"
    assert llm_harness_py.to_json("hello") == '"hello"'


def test_roundtrip_nested():
    data = {"name": "test", "items": [1, 2, {"x": None}], "flag": True}
    json_str = llm_harness_py.to_json(data)
    parsed = json.loads(json_str)
    assert parsed == data


def test_from_json():
    obj = llm_harness_py.from_json('{"a": [1, 2, 3], "b": null}')
    assert obj["a"] == [1, 2, 3]
    assert obj["b"] is None


def test_from_json_roundtrip():
    original = {"nested": {"list": [1, "two", False, None]}}
    json_str = json.dumps(original)
    obj = llm_harness_py.from_json(json_str)
    assert json.loads(llm_harness_py.to_json(obj)) == original
