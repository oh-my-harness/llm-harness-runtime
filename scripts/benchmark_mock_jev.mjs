#!/usr/bin/env node
"use strict";

/**
 * Benchmark: mock Jev-style decision provider vs large-LLM ReAct loop.
 *
 * Demonstrates that constraining decisions to pre-defined choices
 * (Jev-style) can speed up the agent loop by eliminating format errors
 * and reducing LLM calls.
 *
 * Usage:
 *   node scripts/benchmark_mock_jev.mjs --api-key sk-... [--runs 3]
 */

import { OpenAI } from "openai";
import { mkdtempSync, rmSync, writeFileSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { execSync } from "node:child_process";

const args = process.argv.slice(2);
function arg(name, fallback) {
  const i = args.indexOf(`--${name}`);
  return i !== -1 && args[i + 1] ? args[i + 1] : fallback;
}

const API_KEY = arg("api-key", process.env.SF_API_KEY);
const LARGE_MODEL = arg("large-model", "Qwen/Qwen2.5-72B-Instruct");
const RUNS = parseInt(arg("runs", "3"), 10);
if (!API_KEY) { console.error("--api-key required"); process.exit(1); }

const client = new OpenAI({ baseURL: "https://api.siliconflow.cn/v1", apiKey: API_KEY });

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

const TASKS = [
  {
    id: "rename_function",
    prompt: "Read app.py, rename 'old_name' to 'new_name', then verify.",
    setup: (ws) => writeFileSync(join(ws, "app.py"), "def old_name(x):\n    return x * 2\n\nprint(old_name(5))\n"),
    verify: (ws) => { try { const c = readFileSync(join(ws, "app.py"), "utf-8"); return !c.includes("old_name") && c.includes("new_name"); } catch { return false; } },
    // Pre-defined decision plan (what Jev would output as choices)
    plan: [
      { tool: "read_file", args: { path: "app.py" } },
      { tool: "write_file", args: { path: "app.py", content: "# set by write step" } },
      { tool: "grep", args: { pattern: "new_name" } },
      { tool: "bash", args: { cmd: "type app.py" } },
      { tool: "done", args: {} },
    ],
    applyWrite: (observation) => {
      // Simple deterministic transform: replace old_name with new_name
      if (typeof observation === "string" && observation.includes("old_name")) {
        return observation.replace(/old_name/g, "new_name");
      }
      return observation;
    },
  },
  {
    id: "create_readme",
    prompt: "Create README.md with 'Hello World'.",
    setup: () => {},
    verify: (ws) => { try { return readFileSync(join(ws, "README.md"), "utf-8").includes("Hello World"); } catch { return false; } },
    plan: [
      { tool: "write_file", args: { path: "README.md", content: "Hello World" } },
      { tool: "list_files", args: {} },
      { tool: "done", args: {} },
    ],
  },
  {
    id: "add_comment",
    prompt: "Add '# TODO: review' as first line of config.py.",
    setup: (ws) => writeFileSync(join(ws, "config.py"), "DEBUG = True\n"),
    verify: (ws) => { try { return readFileSync(join(ws, "config.py"), "utf-8").startsWith("# TODO: review"); } catch { return false; } },
    plan: [
      { tool: "read_file", args: { path: "config.py" } },
      { tool: "write_file", args: { path: "config.py", content: "# set by write step" } },
      { tool: "done", args: {} },
    ],
    applyWrite: (observation) => "# TODO: review\n" + (observation || ""),
  },
];

// ---------------------------------------------------------------------------
// Tools (Windows-compatible)
// ---------------------------------------------------------------------------

function executeTool(name, toolArgs, ws) {
  try {
    switch (name) {
      case "read_file": return readFileSync(join(ws, toolArgs.path), "utf-8");
      case "write_file": writeFileSync(join(ws, toolArgs.path), toolArgs.content); return `written ${toolArgs.content.length} bytes`;
      case "list_files": return execSync("dir /s /b", { cwd: ws, timeout: 5000, encoding: "utf-8" }).slice(0, 2000) || "(empty)";
      case "grep":
        try { return execSync(`findstr /s /n /c:"${toolArgs.pattern}" *.*`, { cwd: ws, timeout: 5000, encoding: "utf-8" }).slice(0, 4000) || "(no matches)"; }
        catch { return "(no matches)"; }
      case "bash":
        try { return execSync(toolArgs.cmd, { cwd: ws, timeout: 10000, encoding: "utf-8" }).slice(0, 4000) || "(empty)"; }
        catch (e) { return `error: ${e.message}`; }
      default: return `unknown tool: ${name}`;
    }
  } catch (e) { return `error: ${e.message}`; }
}

// ---------------------------------------------------------------------------
// Mode A: Standard ReAct (large LLM, free-form tool selection)
// ---------------------------------------------------------------------------

const SYSTEM_PROMPT = `You are a coding agent on Windows. Available tools:
${JSON.stringify([
  { name: "read_file", description: "Read a file", params: { path: "string" } },
  { name: "write_file", description: "Write a file", params: { path: "string", content: "string" } },
  { name: "list_files", description: "List all files", params: {} },
  { name: "grep", description: "Search for pattern", params: { pattern: "string" } },
  { name: "bash", description: "Run cmd command", params: { cmd: "string" } },
])}

Respond ONLY with JSON (no markdown, no code fences):
{"action":"<tool_name>","args":{...},"done":false}
or
{"action":null,"args":null,"done":true,"summary":"..."}`;

const MAX_STEPS = 10;

async function runLLMAgent(model, task, ws, metrics) {
  const messages = [
    { role: "system", content: SYSTEM_PROMPT },
    { role: "user", content: `Workspace: ${ws}\nTask: ${task.prompt}` },
  ];

  for (let step = 0; step < MAX_STEPS; step++) {
    const t0 = performance.now();
    const resp = await client.chat.completions.create({ model, messages, max_tokens: 512, temperature: 0 });
    const t1 = performance.now();
    metrics.llmCalls++; metrics.llmMs += t1 - t0; metrics.steps++;

    const text = resp.choices?.[0]?.message?.content?.trim() || "";
    let decision;
    try {
      decision = JSON.parse(text.replace(/^```(?:json)?\s*\n?/, "").replace(/\n?```\s*$/, "").trim());
    } catch {
      messages.push({ role: "assistant", content: text });
      messages.push({ role: "user", content: "Invalid JSON. Respond with JSON only." });
      continue;
    }
    if (decision.done) { break; }

    const t2 = performance.now();
    const result = executeTool(decision.action, decision.args || {}, ws);
    const t3 = performance.now();
    metrics.toolCalls++; metrics.toolMs += t3 - t2;

    messages.push({ role: "assistant", content: text });
    messages.push({ role: "user", content: `Tool result:\n${result}` });
  }
  return metrics;
}

// ---------------------------------------------------------------------------
// Mode B: Mock Jev (instant pre-defined choices, zero LLM calls)
// ---------------------------------------------------------------------------

async function runMockJev(task, ws, metrics) {
  const plan = task.plan;
  for (let step = 0; step < plan.length; step++) {
    metrics.steps++;
    const { tool, args: toolArgs } = plan[step];

    if (tool === "done") break;

    // Simulate Jev decision latency (~100ms, no LLM call needed)
    const t0 = performance.now();
    await new Promise((r) => setTimeout(r, 100)); // simulated decision time
    const t1 = performance.now();
    metrics.decisionMs += t1 - t0;
    metrics.decisionCalls++;

    // Execute tool
    let toolArgsFinal = { ...toolArgs };
    if (tool === "write_file" && toolArgs.content === "# set by write step") {
      toolArgsFinal.content = task.applyWrite ? task.applyWrite(metrics.lastResult) : toolArgs.content;
    }

    const t2 = performance.now();
    const result = executeTool(tool, toolArgsFinal, ws);
    const t3 = performance.now();
    metrics.toolCalls++; metrics.toolMs += t3 - t2;
    metrics.lastResult = result;
  }
  return metrics;
}

// ---------------------------------------------------------------------------
// Benchmark runner
// ---------------------------------------------------------------------------

function newMetrics() {
  return { totalMs: 0, llmCalls: 0, llmMs: 0, toolCalls: 0, toolMs: 0, steps: 0, decisionCalls: 0, decisionMs: 0, success: false };
}

async function main() {
  console.log(`\n=== Mock Jev vs LLM Agent Loop Benchmark ===\n`);
  const results = { llm: [], jev: [] };

  for (const [mode, runner] of [
    ["llm", (task, ws, m) => runLLMAgent(LARGE_MODEL, task, ws, m)],
    ["jev", (task, ws, m) => runMockJev(task, ws, m)],
  ]) {
    console.log(`--- ${mode} ---`);
    for (const task of TASKS) {
      for (let i = 0; i < RUNS; i++) {
        const ws = mkdtempSync(join(tmpdir(), "jev_bench_"));
        try {
          task.setup(ws);
          const m = newMetrics();
          const start = performance.now();
          await runner(task, ws, m);
          m.totalMs = performance.now() - start;
          m.success = task.verify(ws);
          m.taskId = task.id; m.mode = mode; m.run = i;
          results[mode].push(m);
          console.log(`  ${task.id} #${i + 1}: ${m.success ? "PASS" : "FAIL"}  ${m.totalMs.toFixed(0)}ms`);
        } catch (e) {
          console.error(`  Error: ${e.message}`);
        } finally {
          try { rmSync(ws, { recursive: true, force: true }); } catch {}
        }
      }
    }
  }

  // Summary
  console.log(`\n=== Summary ===\n`);
  console.log(`Mode | Task              | Success | Avg Time  | LLM Calls | Decision Calls | Tool Calls`);
  console.log(`-----|-------------------|---------|-----------|-----------|----------------|----------`);
  for (const mode of ["llm", "jev"]) {
    const byTask = {};
    for (const r of results[mode]) {
      if (!byTask[r.taskId]) byTask[r.taskId] = [];
      byTask[r.taskId].push(r);
    }
    for (const [taskId, runs] of Object.entries(byTask)) {
      const avg = (key) => (runs.reduce((s, r) => s + (r[key] || 0), 0) / runs.length).toFixed(1);
      const success = runs.every((r) => r.success) ? "100%" : runs.some((r) => r.success) ? "partial" : "0%";
      console.log(
        `${mode.padEnd(4)} | ${taskId.padEnd(17)} | ${success.padStart(7)} | ${(Number(avg("totalMs")) / 1000).toFixed(2).padStart(7)}s | ${avg("llmCalls").padStart(9)} | ${avg("decisionCalls").padStart(14)} | ${avg("toolCalls").padStart(10)}`
      );
    }
  }
}

await main();