#!/usr/bin/env node
"use strict";

/**
 * Proper benchmark: small model with JSON mode (Jev-style constrained decisions)
 * vs large model (72B) — real API calls, real latency, real success rate.
 *
 * Both modes use the SAME SiliconFlow API. Only model + prompt differ.
 *
 * Usage:
 *   node scripts/benchmark_jev_real.mjs --api-key sk-... [--runs 3]
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
const BASE_URL = arg("base-url", "https://api.siliconflow.cn/v1");
const LARGE_MODEL = arg("large-model", "Qwen/Qwen2.5-72B-Instruct");
const SMALL_MODEL = arg("small-model", "Qwen/Qwen3.5-4B");
const RUNS = parseInt(arg("runs", "3"), 10);
if (!API_KEY) { console.error("--api-key required"); process.exit(1); }
const client = new OpenAI({ baseURL: BASE_URL, apiKey: API_KEY });

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

const TASKS = [
  {
    id: "rename_function",
    prompt: "Read app.py, rename 'old_name' to 'new_name' everywhere, then verify the change.",
    setup: (ws) => writeFileSync(join(ws, "app.py"), "def old_name(x):\n    return x * 2\n\nprint(old_name(5))\n"),
    verify: (ws) => { try { const c = readFileSync(join(ws, "app.py"), "utf-8"); return !c.includes("old_name") && c.includes("new_name"); } catch { return false; } },
  },
  {
    id: "add_comment",
    prompt: "Add the comment '# TODO: review' as the first line of config.py.",
    setup: (ws) => writeFileSync(join(ws, "config.py"), "DEBUG = True\n"),
    verify: (ws) => { try { return readFileSync(join(ws, "config.py"), "utf-8").startsWith("# TODO: review"); } catch { return false; } },
  },
  {
    id: "create_readme",
    prompt: "Create README.md containing 'Hello World', then confirm it exists.",
    setup: () => {},
    verify: (ws) => { try { return readFileSync(join(ws, "README.md"), "utf-8").includes("Hello World"); } catch { return false; } },
  },
];

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

const TOOLS_SCHEMA = [
  { name: "read_file", params: { path: "string" }, desc: "read a file" },
  { name: "write_file", params: { path: "string", content: "string" }, desc: "write a file" },
  { name: "list_files", params: {}, desc: "list all files" },
  { name: "grep", params: { pattern: "string" }, desc: "search for pattern" },
  { name: "bash", params: { cmd: "string" }, desc: "run shell command" },
  { name: "done", params: { summary: "string" }, desc: "task complete" },
];

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
      case "done": return "done";
      default: return `unknown tool: ${name}`;
    }
  } catch (e) { return `error: ${e.message}`; }
}

// ---------------------------------------------------------------------------
// Agent loop (shared by both modes, only model differs)
// ---------------------------------------------------------------------------

const MAX_STEPS = 10;

function buildSystemPrompt() {
  return `You are a coding agent on Windows (cmd shell). Tools:
${JSON.stringify(TOOLS_SCHEMA, null, 2)}

Respond ONLY with JSON (no markdown, no fences):
{"action":"<tool_name>","args":{...}}
Set action="done" when finished. Call exactly one tool per turn.`;
}

function newMetrics(model) {
  return { model, totalMs: 0, llmCalls: 0, llmMs: 0, llmLatencies: [], toolCalls: 0, toolMs: 0, steps: 0, formatErrors: 0, success: false, taskId: "", run: 0 };
}

async function runAgent(model, task, ws, metrics) {
  const system = buildSystemPrompt();
  const messages = [
    { role: "system", content: system },
    { role: "user", content: `Workspace: ${ws}\nTask: ${task.prompt}` },
  ];

  for (let step = 0; step < MAX_STEPS; step++) {
    metrics.steps++;

    // Real API call
    const t0 = performance.now();
    const resp = await client.chat.completions.create({
      model,
      messages,
      max_tokens: 256,
      temperature: 0,
      response_format: { type: "json_object" },
    });
    const t1 = performance.now();
    metrics.llmCalls++;
    const lat = t1 - t0;
    metrics.llmMs += lat;
    metrics.llmLatencies.push(lat);

    const text = resp.choices?.[0]?.message?.content?.trim() || "";
    let decision;
    try {
      decision = JSON.parse(text);
    } catch {
      metrics.formatErrors++;
      messages.push({ role: "assistant", content: text });
      messages.push({ role: "user", content: "Invalid JSON. Respond with JSON only." });
      continue;
    }

    const action = decision.action;
    const args = decision.args || {};

    if (action === "done") break;

    if (!TOOLS_SCHEMA.some((t) => t.name === action)) {
      metrics.formatErrors++;
      messages.push({ role: "assistant", content: text });
      messages.push({ role: "user", content: `Unknown tool "${action}". Use one of: ${TOOLS_SCHEMA.map((t) => t.name).join(", ")}` });
      continue;
    }

    const t2 = performance.now();
    const result = executeTool(action, args, ws);
    const t3 = performance.now();
    metrics.toolCalls++;
    metrics.toolMs += t3 - t2;

    messages.push({ role: "assistant", content: text });
    messages.push({ role: "user", content: `Tool result:\n${result}` });
  }
  return metrics;
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

async function main() {
  console.log(`\n=== Real API Benchmark: ${SMALL_MODEL} vs ${LARGE_MODEL} ===`);
  console.log(`Base URL: ${BASE_URL}\n`);

  const results = {};

  for (const [label, model] of [["large", LARGE_MODEL], ["small", SMALL_MODEL]]) {
    results[label] = [];
    console.log(`--- ${label}: ${model} ---`);
    for (const task of TASKS) {
      for (let i = 0; i < RUNS; i++) {
        const ws = mkdtempSync(join(tmpdir(), "jev_real_"));
        try {
          task.setup(ws);
          const m = newMetrics(model);
          m.taskId = task.id;
          m.run = i;
          const start = performance.now();
          await runAgent(model, task, ws, m);
          m.totalMs = performance.now() - start;
          m.success = task.verify(ws);
          results[label].push(m);
          const avgLat = m.llmLatencies.length > 0 ? (m.llmMs / m.llmLatencies.length).toFixed(0) : "-";
          console.log(`  ${task.id} #${i + 1}: ${m.success ? "PASS" : "FAIL"}  ${m.totalMs.toFixed(0)}ms  (LLM ${m.llmCalls}x avg ${avgLat}ms, tools ${m.toolCalls}, fmt_err ${m.formatErrors})`);
        } catch (e) {
          console.error(`  Error: ${e.message}`);
          results[label].push({ taskId: task.id, run: i, model, totalMs: 0, llmCalls: 0, llmMs: 0, llmLatencies: [], toolCalls: 0, toolMs: 0, steps: 0, formatErrors: 0, success: false });
        } finally {
          try { rmSync(ws, { recursive: true, force: true }); } catch {}
        }
      }
    }
  }

  // Summary
  console.log(`\n${"=".repeat(80)}`);
  console.log(`Summary\n`);
  console.log(`Mode  | Task              | Success | Avg Total | Avg LLM/call | LLM calls | Tool calls | Fmt Errors`);
  console.log(`------|-------------------|---------|-----------|--------------|-----------|------------|----------`);

  for (const label of ["large", "small"]) {
    const byTask = {};
    for (const r of results[label]) { (byTask[r.taskId] ??= []).push(r); }
    for (const [taskId, runs] of Object.entries(byTask)) {
      const avg = (k) => runs.reduce((s, r) => s + (r[k] || 0), 0) / runs.length;
      const ok = runs.every((r) => r.success) ? "100%" : runs.some((r) => r.success) ? "partial" : "0%";
      const avgLlmLat = avg("llmCalls") > 0 ? (avg("llmMs") / avg("llmCalls")).toFixed(0) : "-";
      console.log(
        `${label.padEnd(5)} | ${taskId.padEnd(17)} | ${ok.padStart(7)} | ${(avg("totalMs") / 1000).toFixed(2).padStart(8)}s | ${String(avgLlmLat).padStart(12)}ms | ${avg("llmCalls").toFixed(1).padStart(9)} | ${avg("toolCalls").toFixed(1).padStart(10)} | ${avg("formatErrors").toFixed(1).padStart(10)}`
      );
    }
  }
}

await main();