#!/usr/bin/env python3
"""Benchmark: fast decision-model agent loop vs standard large-LLM ReAct loop.

Usage:
    python benchmark_agent_loop.py --base-url https://api.siliconflow.cn/v1 \
        --api-key sk-... --large-model Qwen/Qwen2.5-72B-Instruct \
        --small-model Qwen/Qwen2.5-0.5B-Instruct [--runs 3]

Requires: pip install openai
"""

import argparse
import json
import os
import shutil
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from openai import OpenAI

# ---------------------------------------------------------------------------
# Tools
# ---------------------------------------------------------------------------

def tool_read_file(path: str, workspace: str) -> str:
    full = os.path.join(workspace, path)
    with open(full, "r", encoding="utf-8") as f:
        return f.read()


def tool_write_file(path: str, content: str, workspace: str) -> str:
    full = os.path.join(workspace, path)
    os.makedirs(os.path.dirname(full), exist_ok=True)
    with open(full, "w", encoding="utf-8") as f:
        f.write(content)
    return f"written {len(content)} bytes to {path}"


def tool_list_files(workspace: str) -> str:
    result = []
    for root, dirs, files in os.walk(workspace):
        for f in files:
            rel = os.path.relpath(os.path.join(root, f), workspace)
            result.append(rel)
    return "\n".join(result)


def tool_grep(pattern: str, workspace: str) -> str:
    import subprocess
    try:
        r = subprocess.run(
            ["grep", "-rn", pattern, "."],
            capture_output=True, text=True, cwd=workspace, timeout=10,
        )
        return r.stdout[:4000] or "(no matches)"
    except Exception as e:
        return f"grep error: {e}"


def tool_bash(cmd: str, workspace: str) -> str:
    import subprocess
    try:
        r = subprocess.run(
            cmd, shell=True, capture_output=True, text=True,
            cwd=workspace, timeout=30,
        )
        return (r.stdout + r.stderr)[:4000] or "(empty)"
    except Exception as e:
        return f"error: {e}"


TOOLS = [
    {"name": "read_file", "desc": "Read a file", "params": {"path": "string"}},
    {"name": "write_file", "desc": "Write a file", "params": {"path": "string", "content": "string"}},
    {"name": "list_files", "desc": "List all files", "params": {}},
    {"name": "grep", "desc": "Search for a pattern", "params": {"pattern": "string"}},
    {"name": "bash", "desc": "Run a shell command", "params": {"cmd": "string"}},
]

TOOL_FUNCS = {
    "read_file": lambda args, ws: tool_read_file(args["path"], ws),
    "write_file": lambda args, ws: tool_write_file(args["path"], args["content"], ws),
    "list_files": lambda args, ws: tool_list_files(ws),
    "grep": lambda args, ws: tool_grep(args["pattern"], ws),
    "bash": lambda args, ws: tool_bash(args["cmd"], ws),
}

# ---------------------------------------------------------------------------
# Tasks
# ---------------------------------------------------------------------------

TASKS = [
    {
        "id": "rename_function",
        "prompt": "In the workspace there is a Python file called app.py. Read it, find the function 'old_name', and rename it to 'new_name' everywhere in the file. Then verify by grepping.",
        "setup": lambda ws: _setup_rename(ws),
        "verify": lambda ws: _verify_rename(ws),
    },
    {
        "id": "add_comment",
        "prompt": "Read the file config.py in the workspace and add a comment '# TODO: review' at the top (first line). Then verify by reading the file.",
        "setup": lambda ws: _setup_comment(ws),
        "verify": lambda ws: _verify_comment(ws),
    },
    {
        "id": "create_readme",
        "prompt": "Create a file called README.md in the workspace with the content 'Hello World'. Then list files to confirm.",
        "setup": lambda ws: None,
        "verify": lambda ws: _verify_readme(ws),
    },
]


def _setup_rename(ws):
    Path(ws, "app.py").write_text(
        "def old_name(x):\n    return x * 2\n\nprint(old_name(5))\n", encoding="utf-8"
    )

def _verify_rename(ws):
    content = Path(ws, "app.py").read_text(encoding="utf-8")
    return "old_name" not in content and "new_name" in content

def _setup_comment(ws):
    Path(ws, "config.py").write_text("DEBUG = True\n", encoding="utf-8")

def _verify_comment(ws):
    content = Path(ws, "config.py").read_text(encoding="utf-8")
    return content.startswith("# TODO: review")

def _verify_readme(ws):
    return Path(ws, "README.md").exists() and "Hello World" in Path(ws, "README.md").read_text(encoding="utf-8")


# ---------------------------------------------------------------------------
# Agent Loop
# ---------------------------------------------------------------------------

SYSTEM_PROMPT = """You are a coding agent. You have these tools:
{tools}

Respond ONLY with a JSON object:
{{"action": "<tool_name>", "args": {{...}}, "done": false}}
or {{"action": null, "args": null, "done": true, "summary": "..."}}

Always call exactly one tool per turn. Set done=true when the task is complete."""

MAX_STEPS = 20


class Metrics:
    def __init__(self):
        self.total_time = 0.0
        self.llm_calls = 0
        self.llm_time = 0.0
        self.tool_calls = 0
        self.tool_time = 0.0
        self.steps = 0

    def to_dict(self):
        return {
            "total_time_s": round(self.total_time, 3),
            "llm_calls": self.llm_calls,
            "llm_time_s": round(self.llm_time, 3),
            "tool_calls": self.tool_calls,
            "tool_time_s": round(self.tool_time, 3),
            "steps": self.steps,
        }


def run_agent(client: OpenAI, model: str, prompt: str, workspace: str, metrics: Metrics) -> bool:
    system = SYSTEM_PROMPT.format(tools=json.dumps(TOOLS, indent=2))
    messages = [
        {"role": "system", "content": system},
        {"role": "user", "content": prompt},
    ]

    for step in range(MAX_STEPS):
        metrics.steps += 1

        # LLM call
        t0 = time.monotonic()
        resp = client.chat.completions.create(
            model=model,
            messages=messages,
            max_tokens=512,
            temperature=0.0,
        )
        t1 = time.monotonic()
        metrics.llm_calls += 1
        metrics.llm_time += t1 - t0

        text = resp.choices[0].message.content or ""
        try:
            decision = json.loads(text)
        except json.JSONDecodeError:
            messages.append({"role": "assistant", "content": text})
            messages.append({"role": "user", "content": "Invalid JSON. Please respond with valid JSON only."})
            continue

        if decision.get("done"):
            messages.append({"role": "assistant", "content": text})
            break

        action = decision.get("action")
        args = decision.get("args", {})

        if action not in TOOL_FUNCS:
            messages.append({"role": "assistant", "content": text})
            messages.append({"role": "user", "content": f"Unknown tool '{action}'. Available: {[t['name'] for t in TOOLS]}"})
            continue

        # Tool execution
        t2 = time.monotonic()
        try:
            result = TOOL_FUNCS[action](args, workspace)
        except Exception as e:
            result = f"error: {e}"
        t3 = time.monotonic()
        metrics.tool_calls += 1
        metrics.tool_time += t3 - t2

        messages.append({"role": "assistant", "content": text})
        messages.append({"role": "user", "content": f"Tool result:\n{result}"})

    metrics.total_time = metrics.llm_time + metrics.tool_time
    return True  # verified externally


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def run_benchmark(args):
    client = OpenAI(base_url=args.base_url, api_key=args.api_key)
    results = {}

    for mode, model in [("baseline", args.large_model), ("fast", args.small_model)]:
        results[mode] = []
        for task in TASKS:
            task_results = []
            for run in range(args.runs):
                ws = tempfile.mkdtemp(prefix="agent_bench_")
                try:
                    if task["setup"]:
                        task["setup"](ws)
                    m = Metrics()
                    run_agent(client, model, task["prompt"], ws, m)
                    success = task["verify"](ws)
                    entry = m.to_dict()
                    entry["success"] = success
                    entry["task_id"] = task["id"]
                    entry["run"] = run
                    task_results.append(entry)
                finally:
                    shutil.rmtree(ws, ignore_errors=True)
            results[mode].append(task_results)

    # Print summary
    print(f"\n{'Model':<12} {'Task':<16} {'Success':>8} {'Avg Time':>10} {'LLM Calls':>10} {'Tool Calls':>11}")
    print("-" * 60)
    for mode, model in [("baseline", args.large_model), ("fast", args.small_model)]:
        for task_idx, task_runs in enumerate(results[mode]):
            successes = sum(1 for r in task_runs if r["success"])
            avg_time = sum(r["total_time_s"] for r in task_runs) / len(task_runs)
            avg_llm = sum(r["llm_calls"] for r in task_runs) / len(task_runs)
            success_pct = f"{100 * successes / len(task_runs):.0f}%"
            print(f"{mode:<12} {TASKS[task_idx]['id']:<16} {success_pct:>8} {avg_time:>8.2f}s {avg_llm:>10.1f}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Benchmark agent loop speed: large vs fast model")
    parser.add_argument("--base-url", default="https://api.siliconflow.cn/v1")
    parser.add_argument("--api-key", required=True)
    parser.add_argument("--large-model", default="Qwen/Qwen2.5-72B-Instruct")
    parser.add_argument("--small-model", default="Qwen/Qwen2.5-0.5B-Instruct")
    parser.add_argument("--runs", type=int, default=3)
    args = parser.parse_args()
    run_benchmark(args)
