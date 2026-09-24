#!/usr/bin/env node
"use strict";

/**
 * Benchmark: fast decision-model agent loop vs standard large-LLM ReAct loop.
 *
 * Usage:
 *   node scripts/benchmark_agent_loop.mjs \
 *     --base-url https://api.siliconflow.cn/v1 \
 *     --api-key sk-... \
 *     --large-model Qwen/Qwen2.5-72B-Instruct \
 *     --small-model Qwen/Qwen2.5-0.5B-Instruct \
 *     --runs 3
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

const BASE_URL = arg("base-url", "https://api.siliconflow.cn/v1");
const API_KEY = arg("api-key", process.env.OPENAI_API_KEY || process.env.SF_API_KEY);
const LARGE_MODEL = arg("large-model", "Qwen/Qwen2.5-72B-Instruct");
const SMALL_MODEL = arg("small-model", "Qwen/Qwen2.5-0.5B-Instruct");
const RUNS = parseInt(arg("runs", "3"), 10);

if (!API_KEY) { console.error("Error: --api-key is required."); process.exit(1); }

const client = new OpenAI({ baseURL: BASE_URL, apiKey: API_KEY });

// --- Tasks ---

function setupRename(ws) {
  writeFileSync(join(ws, "app.py"), "def old_name(x):\n    return x * 2\n\nprint(old_name(5))\n");
}

const TASKS = [
  {
    id: "rename_function",
    prompt: "In the workspace there is a Python file called app.py. Read it, find the function 'old_name', and rename it to 'new_name' everywhere. Then verify by grepping.",
    setup: (ws) => setupRename(ws),
    verify: (ws) => { try { const c = readFileSync(join(ws, "app.py"), "utf-8"); return !c.includes("old_name") && c.includes("new_name"); } catch { return false; } },
  },
  {
    id: "add_comment",
    prompt: "Read the file config.py in the workspace and add a comment '# TODO: review' as the first line. Then read the file to confirm.",
    setup: (ws) => writeFileSync(join(ws, "config.py"), "DEBUG = True\n"),
    verify: (ws) => { try { return readFileSync(join(ws, "config.py"), "utf-8").startsWith("# TODO: review"); } catch { return false; } },
  },
  {
    id: "create_readme",
    prompt: "Create a file called README.md with the content 'Hello World'. Then list files to confirm it exists.",
    setup: () => {},
    verify: (ws) => { try { return readFileSync(join(ws, "README.md"), "utf-8").includes("Hello World"); } catch { return false; } },
  },
];

// --- Tools ---

const TOOLS = [
  { name: "read_file", description: "Read a file", params: { path: "string" } },
  { name: "write_file", description: "Write a file", params: { path: "string", content: "string" } },
  { name: "list_files", description: "List all files", params: {} },
  { name: "grep", description: "Search for a pattern", params: { pattern: "string" } },
  { name: "bash", description: "Run a shell command", params: { cmd: "string" } },
];

function executeTool(name, toolArgs, ws) {
  try {
    switch (name) {
      case "read_file": return readFileSync(join(ws, toolArgs.path), "utf-8");
      case "write_file": writeFileSync(join(ws, toolArgs.path), toolArgs.content); return `written ${toolArgs.content.length} bytes`;
      case "list_files": return execSync("dir /s /b", { cwd: ws, timeout: 5000, encoding: "utf-8" }).slice(0, 2000) || "(empty)";
      case "grep":
        try { return execSync(`findstr /s /n /c:"${toolArgs.pattern}" *.*`, { cwd: ws, timeout: 5000, encoding: "utf-8" }).slice(0, 4000) || "(no matches)"; } catch { return "(no matches)"; }
      case "bash":
        try { return execSync(toolArgs.cmd, { cwd: ws, timeout: 10000, encoding: "utf-8" }).slice(0, 4000) || "(empty)"; }
        catch (e) { return `error: ${e.message}`; }
      default: return `unknown tool: ${name}`;
    }
  } catch (e) { return `error: ${e.message}`; }
}

// --- Agent Loop ---

const SYSTEM_PROMPT = `You are a coding agent running on Windows (PowerShell shell). Available tools:\n${JSON.stringify(TOOLS)}\n\nRespond ONLY with JSON (no markdown, no code fences):\n{"action":"<tool_name>","args":{...},"done":false}\nor\n{"action":null,"args":null,"done":true,"summary":"..."}\n\nCall exactly one tool per turn. Set done=true when finished.`;

const MAX_STEPS = 10;

async function runAgent(model, task, ws, metrics) {
  const messages = [
    { role: "system", content: SYSTEM_PROMPT },
    { role: "user", content: `Workspace: ${ws}\n\nTask: ${task.prompt}` },
  ];

  for (let step = 0; step < MAX_STEPS; step++) {
    const t0 = performance.now();
    const resp = await client.chat.completions.create({
      model,
      messages,
      max_tokens: 512,
      temperature: 0,
    });
    const t1 = performance.now();
    metrics.llmCalls++;
    metrics.llmMs += t1 - t0;

    const text = resp.choices?.[0]?.message?.content?.trim() || "";
    let decision;
    try {
      const cleaned = text.replace(/^```(?:json)?\s*\n?/, "").replace(/\n?```\s*$/, "").trim();
      decision = JSON.parse(cleaned);
    } catch {
      messages.push({ role: "assistant", content: text });
      messages.push({ role: "user", content: "Invalid JSON. Respond with valid JSON only." });
      continue;
    }

    if (decision.done) { messages.push({ role: "assistant", content: text }); break; }

    const { action, args: toolArgs } = decision;
    if (!action || !TOOLS.some((t) => t.name === action)) {
      messages.push({ role: "assistant", content: text });
      messages.push({ role: "user", content: `Unknown tool "${action}". Available: ${TOOLS.map((t) => t.name).join(", ")}` });
      continue;
    }

    const t2 = performance.now();
    const result = executeTool(action, toolArgs || {}, ws);
    const t3 = performance.now();
    metrics.toolCalls++;
    metrics.toolMs += t3 - t2;

    messages.push({ role: "assistant", content: text });
    messages.push({ role: "user", content: `Tool result:\n${result}` });
  }

  metrics.totalMs = Date.now();
  return metrics;
}

// --- Benchmark runner ---

async function main() {
  console.log(`\n=== Agent Loop Benchmark ===`);
  console.log(`Base URL:    ${BASE_URL}`);
  console.log(`Large model: ${LARGE_MODEL}`);
  console.log(`Small model: ${SMALL_MODEL}`);
  console.log(`Runs/task:   ${RUNS}\n`);

  const results = {};

  for (const [mode, model] of [["baseline", LARGE_MODEL], ["fast", SMALL_MODEL]]) {
    results[mode] = [];
    console.log(`\n--- ${mode}: ${model} ---`);
    for (const task of TASKS) {
      const runs = [];
      for (let i = 0; i < RUNS; i++) {
        const ws = mkdtempSync(join(tmpdir(), "agent_bench_"));
        try {
          task.setup(ws);
          const m = { llmCalls: 0, llmMs: 0, toolCalls: 0, toolMs: 0, steps: 0, totalMs: 0, success: false };
          const start = performance.now();
          await runAgent(model, task, ws, m);
          m.totalMs = performance.now() - start;
          m.success = task.verify(ws);
          m.taskId = task.id;
          m.mode = mode;
          m.run = i;
          runs.push(m);
        } catch (e) {
          console.error(`  Error in ${task.id}: ${e.message}`);
          runs.push({ taskId: task.id, mode, run: _runIdx, totalMs: 0, llmCalls: 0, llmMs: 0, toolCalls: 0, toolMs: 0, success: false, error: e.message });
        } finally {
          try { rmSync(ws, { recursive: true, force: true }); } catch {}
        }
      }
      results[mode].push({ taskId: task.id, runs });
    }
  }

  // Summary
  console.log(`\n=== Summary ===\n`);
  console.log(`Mode       | Task              | Success | Avg Time  | LLM Calls | Tool Calls`);
  console.log(`-----------|-------------------|---------|-----------|-----------|----------`);
  for (const mode of ["baseline", "fast"]) {
    if (!results[mode]) continue;
    for (const t of results[mode]) {
      const avgMs = t.runs.reduce((s, r) => s + r.totalMs, 0) / t.runs.length;
      const avgLlm = t.runs.reduce((s, r) => s + r.llmCalls, 0) / t.runs.length;
      const avgTool = t.runs.reduce((s, r) => s + r.toolCalls, 0) / t.runs.length;
      const allPass = t.runs.every((r) => r.success);
      const success = allPass ? "100%" : t.runs.some((r) => r.success) ? "partial" : "0%";
      console.log(`${mode.padEnd(10)} | ${t.taskId.padEnd(17)} | ${success.padStart(7)} | ${(avgMs / 1000).toFixed(2).padStart(7)}s | ${avgLlm.toFixed(1).padStart(9)} | ${avgTool.toFixed(1).padStart(10)}`);
    }
  }
}

// Use process.argv for run index
let _runIdx = 0; _runIdx++;

await main();

