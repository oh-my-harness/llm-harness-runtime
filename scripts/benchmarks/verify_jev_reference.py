"""Offline verification of the vendored adaptation against a pinned upstream checkout."""

import argparse
import ast
import hashlib
import json
from pathlib import Path
import subprocess


REVISION = "1231850a0bf1a0c0341fe408ef1668dbbfdfac46"


def verify(checkout):
    revision = subprocess.check_output(["git", "-C", str(checkout), "rev-parse", "HEAD"], text=True).strip()
    if revision != REVISION:
        raise ValueError("Unexpected upstream commit")
    local = Path(__file__).with_name("jev_reference")
    source = checkout / "jev_ultrafast"
    originals = {}
    for name in ("snapshot.js", "questions.py", "model.py", "browser.py"):
        # Verify both the working copy and its clean upstream blob, not just HEAD's name.
        original = subprocess.check_output(["git", "-C", str(checkout), "show", f"{REVISION}:jev_ultrafast/{name}"])
        text = original.decode("utf-8").replace("\r\n", "\n")
        if (source / name).read_text(encoding="utf-8") != text:
            raise ValueError("Upstream source checkout has local changes: " + name)
        originals[name] = text
    for name in ("snapshot.js", "questions.py"):
        if (local / name).read_text(encoding="utf-8") != originals[name]:
            raise ValueError("Reference file differs from upstream: " + name)
    original_license = subprocess.check_output(["git", "-C", str(checkout), "show", f"{REVISION}:LICENSE"]).decode()
    if (local / "LICENSE").read_text(encoding="utf-8") != original_license.replace("\r\n", "\n"):
        raise ValueError("Missing or modified MIT license")
    model = ast.parse(originals["model.py"])
    functions = {node.name: node for node in model.body if isinstance(node, ast.FunctionDef)}
    protocol = ast.parse((local / "protocol.py").read_text(encoding="utf-8"))
    extracted = {node.name: node for node in protocol.body if isinstance(node, ast.FunctionDef)}
    for name in ("validate_choice", "action_space", "field_context"):
        if ast.dump(functions[name]) != ast.dump(extracted[name]):
            raise ValueError("Extracted function changed: " + name)
    choose = functions["choose"]
    choose.name = "build_request"
    body_index = next(i for i, node in enumerate(choose.body)
                      if isinstance(node, ast.Assign) and isinstance(node.targets[0], ast.Name)
                      and node.targets[0].id == "body")
    choose.body = choose.body[:body_index + 1]
    body = choose.body[-1].value
    model_index = next(i for i, key in enumerate(body.keys) if key.value == "model")
    del body.keys[model_index]
    del body.values[model_index]
    choose.body.append(ast.Return(value=ast.Name(id="body", ctx=ast.Load())))
    if ast.dump(choose) != ast.dump(extracted["build_request"]):
        raise ValueError("Decision request differs beyond removing the provider model name and transport")
    expected = originals["browser.py"].replace(
        "from browser_harness.admin import ensure_daemon\nfrom browser_harness.helpers import cdp\n", "")
    start, end = expected.index("    def __init__(self, url):"), expected.index("    def call(")
    expected = expected[:start] + "    def __init__(self, session):\n        self.session = session\n\n" + expected[end:]
    expected = expected.replace("return cdp(method, session_id=self.session, **params)", "return self.session.send(method, params)")
    expected = expected.replace("return cdp(method, session_id=session, **params)", "return session.send(method, params)")
    expected = expected.replace('        if self.target:\n            cdp("Target.closeTarget", targetId=self.target)\n            self.target = None',
                                "        self.session.detach()")
    expected = expected.replace('.read_text()', '.read_text(encoding="utf-8")')
    expected_tree = ast.parse(expected)
    local_tree = ast.parse((local / "browser.py").read_text(encoding="utf-8"))
    expected_tree.body.pop(0)
    local_tree.body.pop(0)
    if ast.dump(expected_tree) != ast.dump(local_tree):
        raise ValueError("Browser adaptation changed beyond documented transport and UTF-8 changes")
    return {"verified": True, "upstream_commit": REVISION,
            "upstream_sha256_lf": {name: hashlib.sha256(text.encode()).hexdigest() for name, text in originals.items()},
            "unchanged_files": ["snapshot.js", "questions.py", "LICENSE"],
            "unchanged_functions": ["validate_choice", "action_space", "field_context"]}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--checkout", type=Path, required=True)
    args = parser.parse_args()
    print(json.dumps(verify(args.checkout), indent=2))
