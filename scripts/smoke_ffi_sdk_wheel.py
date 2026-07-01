#!/usr/bin/env python
from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Install an llm-harness SDK wheel into a temporary target and verify native ABI loading."
    )
    parser.add_argument("wheel", type=Path, help="Path to the wheel file to smoke test")
    parser.add_argument("--expected-abi", type=int, default=2)
    parser.add_argument(
        "--keep-target",
        action="store_true",
        help="Keep the temporary install directory for manual inspection",
    )
    args = parser.parse_args()

    wheel = args.wheel.resolve()
    if not wheel.exists():
        print(f"wheel not found: {wheel}", file=sys.stderr)
        return 2

    target = Path(tempfile.mkdtemp(prefix="llm-harness-sdk-wheel-"))
    try:
        subprocess.run(
            [
                sys.executable,
                "-m",
                "pip",
                "install",
                "--quiet",
                "--target",
                str(target),
                str(wheel),
            ],
            check=True,
        )

        env = os.environ.copy()
        env["PYTHONPATH"] = (
            str(target)
            if not env.get("PYTHONPATH")
            else str(target) + os.pathsep + env["PYTHONPATH"]
        )
        code = f"""
from llm_harness_sdk import native_abi_version, native_version

abi = native_abi_version()
version = native_version()
if abi != {args.expected_abi}:
    raise SystemExit(f"unexpected ABI version: {{abi}}")
if not version:
    raise SystemExit("native version is empty")
print(f"llm_harness_sdk wheel smoke ok: ABI={{abi}}, native={{version}}")
"""
        subprocess.run([sys.executable, "-c", code], check=True, env=env)
    finally:
        if args.keep_target:
            print(f"kept smoke install target: {target}")
        else:
            shutil.rmtree(target, ignore_errors=True)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
