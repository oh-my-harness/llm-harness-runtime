#!/usr/bin/env node
"use strict";

/**
 * Benchmark: state-based rule decision provider (simulates Jev) vs LLM ReAct.
 *
 * Unlike the hardcoded mock, this makes decisions based on actual workspace
 * state each step — closer to how a real Jev model would work.
 *
 * Usage:
 *   node scripts/benchmark_state_jev.mjs --api-key sk-... [--runs 3]
 */

import { OpenAI } from "openai";
import { mkdtempSync, rmSync, writeFileSync, readFileSync, existsSync } from "node:fs";
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
// Tools
// ---------------------------------------------------------------------------

function executeTool(name, toolArgs, ws) {
  try {
    switch (name) {
      case "read_file": return readFileSync(join(ws, toolArgs.path), "utf-8");
      case "write_file": writeFileSync(join(ws, toolArgs.path), toolArgs.content); return `written ${toolArgs.content.length} bytes to ${toolArgs.path}`;
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
// State-based Decision Rules (simulates Jev)
// ---------------------------------------------------------------------------

/**
 * Each task defines a `decide(state, workspace)` function that inspects
 * actual state and returns { action, args } or { done: true }.
 * This mirrors Jev: given state, pick from pre-defined options.
 */

const TASKS = [
  {
    id: "rename_function",
    prompt: "Read app.py, rename 'old_name' to 'new_name', verify.",
    setup: (ws) => writeFileSync(join(ws, "app.py"), "def old_name(x):\n    return x * 2\n\nprint(old_name(5))\n"),
    verify: (ws) => { try { const c = readFileSync(join(ws, "app.py"), "utf-8"); return !c.includes("old_name") && c.includes("new_name"); } catch { return false; } },

    decide: (ws, lastResult) => {
      const path = join(ws, "app.py");
      if (!existsSync(path)) return { done: true, reason: "file missing" };
      const content = readFileSync(path, "utf-8");
      if (content.includes("new_name") && !content.includes("old_name")) return { done: true };
      if (lastResult && typeof lastResult === "string" && lastResult.includes("old_name")) {
        return { action: "write_file", args: { path: "app.py", content: lastResult.replace(/old_name/g, "new_name") } };
      }
      return { action: "read_file", args: { path: "app.py" } };
    },
  },
  {
    id: "add_comment",
    prompt: "Add '# TODO: review' as first line of config.py.",
    setup: (ws) => writeFileSync(join(ws, "config.py"), "DEBUG = True\n"),
    verify: (ws) => { try { return readFileSync(join(ws, "config.py"), "utf-8").startsWith("# TODO: review"); } catch { return false; } },
    decide: (ws, lastResult) => {
      const path = join(ws, "config.py");
      if (!existsSync(path)) return { done: true, reason: "file missing" };
      const content = readFileSync(path, "utf-8");
      if (content.startsWith("# TODO: review")) return { done: true };
      // Read content first, then write with prefix
      return { action: "write_file", args: { path: "config.py", content: "# TODO: review\n" + content } };
    },
  },
  {
    id: "create_readme",
    prompt: "Create README.md with 'Hello World'.",
    setup: () => {},
    verify: (ws) => { try { return readFileSync(join(ws, "README.md"), "utf-8").includes("Hello World"); } catch { return false; } },
    decide: (ws) => {
      const path = join(ws, "README.md");
      if (!existsSync(path)) return { action: "write_file", args: { path: "README.md", content: "Hello World" } };
      const c = readFileSync(path, "utf-8");
      if (c.includes("Hello World")) return { done: true };
      return { action: "write_file", args: { path: "README.md", content: "Hello World" } };
    },
  },
];

// ---------------------------------------------------------------------------
// State-Jev Runner
// ---------------------------------------------------------------------------

const DECISION_LATENCY_MS = 100; // simulate Jev's ~100ms

async function runStateJev(task, ws, metrics) {
  let lastResult = null;
  for (let step = 0; step < 10; step++) {
    metrics.steps++;

    // Decision (simulated ~100ms, no LLM call)
    const t0 = performance.now();
    await new Promise((r) => setTimeout(r, DECISION_LATENCY_MS));
    const decision = task.decide(ws, lastResult);
    const t1 = performance.now();
    metrics.decisionCalls++; metrics.decisionMs += t1 - t0;

    if (decision.done) break;

    const t2 = performance.now();
    const result = executeTool(decision.action, decision.args, ws);
    const t3 = performance.now();
    metrics.toolCalls++; metrics.toolMs += t3 - t2;
    lastResult = result;
  }
  return metrics;
}

// ---------------------------------------------------------------------------
// LLM Runner (baseline)
// ---------------------------------------------------------------------------

const SYSTEM_PROMPT = `You are a coding agent on Windows. Tools: read_file(path), write_file(path,content), list_files(), grep(pattern), bash(cmd).
Respond ONLY with JSON: {"action":"<tool>","args":{...},"done":false} or {"action":null,"done":true,"summary":"..."}
Call one tool per turn.`;

async function runLLM(task, ws, metrics) {
  const messages = [
    { role: "system", content: SYSTEM_PROMPT },
    { role: "user", content: `Workspace: ${ws}\nTask: ${task.prompt}` },
  ];
  for (let step = 0; step < 10; step++) {
    const t0 = performance.now();
    const resp = await client.chat.completions.create({ model: LARGE_MODEL, messages, max_tokens: 512, temperature: 0 });
    const t1 = performance.now();
    metrics.llmCalls++; metrics.llmMs += t1 - t0; metrics.steps++;
    const text = resp.choices?.[0]?.message?.content?.trim() || "";
    let d;
    try { d = JSON.parse(text.replace(/^```(?:json)?\s*\n?/, "").replace(/\n?```\s*$/, "").trim()); }
    catch { messages.push({ role: "assistant", content: text }, { role: "user", content: "Invalid JSON." }); continue; }
    if (d.done) break;
    const t2 = performance.now();
    const result = executeTool(d.action, d.args || {}, ws);
    const t3 = performance.now();
    metrics.toolCalls++; metrics.toolMs += t3 - t2;
    messages.push({ role: "assistant", content: text }, { role: "user", content: `Result:\n${result}` });
  }
  return metrics;
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

function newMetrics() {
  return { totalMs: 0, llmCalls: 0, llmMs: 0, toolCalls: 0, toolMs: 0, steps: 0, decisionCalls: 0, decisionMs: 0, success: false, taskId: "", mode: "", run: 0 };
}

async function main() {
  console.log(`\n=== State-based Jev vs LLM Benchmark ===\n`);
  const results = { llm: [], jev: [] };

  for (const [mode, runner] of [["llm", runLLM], ["jev", runStateJev]]) {
    console.log(`--- ${mode} ---`);
    for (const task of TASKS) {
      for (let i = 0; i < RUNS; i++) {
        const ws = mkdtempSync(join(tmpdir(), "sjev_"));
        try {
          task.setup(ws);
          const m = newMetrics();
          const start = performance.now();
          await runner(task, ws, m);
          m.totalMs = performance.now() - start;
          m.success = task.verify(ws);
          m.taskId = task.id; m.mode = mode; m.run = i;
          results[mode].push(m);
          console.log(`  ${task.id} #${i + 1}: ${m.success ? "PASS" : "FAIL"} ${m.totalMs.toFixed(0)}ms`);
        } catch (e) { console.error(`  Error: ${e.message}`); }
        finally { try { rmSync(ws, { recursive: true, force: true }); } catch {} }
      }
    }
  }

  console.log(`\n=== Summary ===\n`);
  console.log(`Mode | Task              | Success | Avg Time  | LLM | Decisions | Tools`);
  console.log(`-----|-------------------|---------|-----------|-----|-----------|-----`);
  for (const mode of ["llm", "jev"]) {
    const byTask = {};
    for (const r of results[mode]) { (byTask[r.taskId] ??= []).push(r); }
    for (const [taskId, runs] of Object.entries(byTask)) {
      const avg = (k) => (runs.reduce((s, r) => s + (r[k] || 0), 0) / runs.length).toFixed(1);
      const ok = runs.every((r) => r.success) ? "100%" : runs.some((r) => r.success) ? "partial" : "0%";
      console.log(`${mode.padEnd(4)} | ${taskId.padEnd(17)} | ${ok.padStart(7)} | ${(Number(avg("totalMs")) / 1000).toFixed(2).padStart(7)}s | ${avg("llmCalls").padStart(3)} | ${avg("decisionCalls").padStart(9)} | ${avg("toolCalls").padStart(5)}`);
    }
  }
}

await main();
