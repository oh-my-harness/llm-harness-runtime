#!/usr/bin/env node
"use strict";

/**
 * Compare large and small models in the same ReAct loop:
 * real API latency, call counts, and independently verified task accuracy.
 *
 * Independent endpoints, identical prompts/tools, and recorded warmup timings.
 *
 * Usage:
 *   Set LARGE_API_KEY in the environment, then run with --mode both.
 */

import { OpenAI } from "openai";
import { mkdtempSync, rmSync, writeFileSync, readFileSync, readdirSync, mkdirSync } from "node:fs";
import { summarizeResults } from "./benchmark_metrics.mjs";
import { expandedTasks, suiteHash, suiteDefinition, buildSchedule } from "./benchmark_suite.mjs";
import { join, resolve, relative, isAbsolute, dirname } from "node:path";
import { tmpdir } from "node:os";

const args = process.argv.slice(2);
function arg(name, fallback) {
  const i = args.indexOf(`--${name}`);
  return i !== -1 && args[i + 1] ? args[i + 1] : fallback;
}
const API_KEY = arg("api-key", process.env.SF_API_KEY);
const BASE_URL = arg("base-url", "https://api.siliconflow.cn/v1");
const LARGE_MODEL = arg("large-model", "Qwen/Qwen2.5-72B-Instruct");
const SMALL_MODEL = arg("small-model", "Qwen/Qwen3.5-4B");
const RUNS = Number(arg("runs", "3"));
const TIMEOUT = Number(arg("timeout-ms", "60000"));
const WARMUPS = Number(arg("warmups", "1"));
const OUTPUT = resolve(arg("output", `logs/benchmarks/jev-${Date.now()}.json`));
const MODE = arg("mode", "both");
const SUITE = arg("suite", "smoke");
if (!["smoke", "expanded"].includes(SUITE)) throw new Error("Invalid --suite");
if (args.includes("--policy")) throw new Error("--policy is no longer supported; this benchmark compares large and small models in the same ReAct loop");
if (!["both", "large", "small"].includes(MODE)) throw new Error("Invalid --mode");
if (!Number.isInteger(RUNS) || RUNS < 1 || !Number.isInteger(WARMUPS) || WARMUPS < 0 || !Number.isFinite(TIMEOUT) || TIMEOUT <= 0) throw new Error("Invalid runs, warmups, or timeout");
const configs = Object.entries({ large: LARGE_MODEL, small: SMALL_MODEL })
  .filter(([label]) => MODE === "both" || MODE === label)
  .map(([label, model]) => {
    const baseURL = arg(`${label}-base-url`, BASE_URL);
    const url = new URL(baseURL);
    if (url.username || url.password || url.search) throw new Error("Endpoint credentials must be supplied via environment");
    const apiKey = process.env[`${label.toUpperCase()}_API_KEY`] || API_KEY || (url.hostname === "127.0.0.1" || url.hostname === "localhost" ? "ollama" : undefined);
    if (!apiKey) throw new Error(`Missing ${label.toUpperCase()}_API_KEY or SF_API_KEY`);
    const thinking = arg(`${label}-thinking`, "default");
    if (!["default", "off"].includes(thinking)) throw new Error("Invalid thinking mode");
    return { label, model, baseURL, thinking, client: new OpenAI({ baseURL, apiKey, timeout: TIMEOUT, maxRetries: 0 }) };
  });

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

const SMOKE_TASKS = [
  {
    id: "rename_function",
    prompt: "Read app.py, rename 'old_name' to 'new_name' everywhere, then verify the change.",
    setup: (ws) => writeFileSync(join(ws, "app.py"), "def old_name(x):\n    return x * 2\n\nprint(old_name(5))\n"),
    verify: (ws) => { try { return readFileSync(join(ws, "app.py"), "utf-8").replaceAll("\r\n", "\n").replace(/\n$/, "") === "def new_name(x):\n    return x * 2\n\nprint(new_name(5))"; } catch { return false; } },
  },
  {
    id: "add_comment",
    prompt: "Add the comment '# TODO: review' as the first line of config.py.",
    setup: (ws) => writeFileSync(join(ws, "config.py"), "DEBUG = True\n"),
    verify: (ws) => { try { return readFileSync(join(ws, "config.py"), "utf-8").replaceAll("\r\n", "\n").replace(/\n$/, "") === "# TODO: review\nDEBUG = True"; } catch { return false; } },
  },
  {
    id: "create_readme",
    prompt: "Create README.md containing 'Hello World', then confirm it exists.",
    setup: () => {},
    verify: (ws) => { try { return readFileSync(join(ws, "README.md"), "utf-8").includes("Hello World"); } catch { return false; } },
  },
];
const TASKS = SUITE === "expanded" ? expandedTasks : SMOKE_TASKS;

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

const TOOLS_SCHEMA = [
  { name: "read_file", params: { path: "string" }, desc: "read a file" },
  { name: "write_file", params: { path: "string", content: "string" }, desc: "write a file" },
  { name: "list_files", params: {}, desc: "list all files" },
  { name: "grep", params: { pattern: "string" }, desc: "search for pattern" },
  { name: "done", params: { summary: "string" }, desc: "task complete" },
];

function executeTool(name, toolArgs, ws) {
  const workspacePath = (path) => {
    if (typeof path !== "string" || isAbsolute(path)) throw new Error("Expected relative file path");
    const target = resolve(ws, path);
    const rel = relative(ws, target);
    if (rel === ".." || rel.startsWith("../") || rel.startsWith("..\\") || isAbsolute(rel)) throw new Error("Path outside workspace");
    return target;
  };
  try {
    switch (name) {
      case "read_file": return readFileSync(workspacePath(toolArgs.path), "utf-8");
      case "write_file": writeFileSync(workspacePath(toolArgs.path), toolArgs.content); return `written ${toolArgs.content.length} bytes to ${toolArgs.path}`;
      case "list_files": return readdirSync(ws).join("\n") || "(empty)";
      case "grep":
        return readdirSync(ws, { withFileTypes: true }).filter((f) => f.isFile()).flatMap((f) => readFileSync(join(ws, f.name), "utf-8").split("\n").flatMap((line, i) => line.includes(toolArgs.pattern) ? [`${f.name}:${i + 1}:${line}`] : [])).join("\n").slice(0, 4000) || "(no matches)";
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
  return `You are a coding agent. Use relative file paths. Tools:
${JSON.stringify(TOOLS_SCHEMA, null, 2)}

Respond ONLY with JSON (no markdown, no fences):
{"action":"<tool_name>","args":{...}}
Set action="done" when finished. Call exactly one tool per turn.`;
}

function newMetrics(model) {
  return { model, totalMs: 0, llmCalls: 0, llmMs: 0, llmLatencies: [], toolCalls: 0, toolMs: 0, toolLatencies: [], steps: 0, formatErrors: 0, toolErrors: 0, success: false, taskId: "", run: 0, termination: "max_steps", trace: [] };
}

async function runAgent(config, task, ws, metrics) {
  let faultPending = Boolean(task.fault);
  metrics.injectedFaults = 0;
  const system = buildSystemPrompt();
  const messages = [
    { role: "system", content: system },
    { role: "user", content: `Task: ${task.prompt}` },
  ];

  for (let step = 0; step < MAX_STEPS; step++) {
    metrics.steps++;

    // Real API call
    const t0 = performance.now();
    metrics.llmCalls++;
    let resp;
    try {
      resp = await config.client.chat.completions.create({
        model: config.model,
        messages,
        max_tokens: 256,
        temperature: 0,
        response_format: { type: "json_object" },
        ...(config.thinking === "off" ? { reasoning_effort: "none" } : {}),
      });
    } finally {
      const lat = performance.now() - t0;
      metrics.llmMs += lat;
      metrics.llmLatencies.push(lat);
    }

    const text = resp.choices?.[0]?.message?.content?.trim() || "";
    metrics.trace.push({ source: "model", step, text, usage: resp.usage, finishReason: resp.choices?.[0]?.finish_reason });
    let decision;
    try {
      decision = JSON.parse(text);
      if (!decision || typeof decision !== "object" || Array.isArray(decision)) throw new Error("Expected decision object");
    } catch {
      metrics.formatErrors++;
      messages.push({ role: "assistant", content: text });
      messages.push({ role: "user", content: "Invalid JSON. Respond with JSON only." });
      continue;
    }

    const action = decision.action;
    const args = decision.args || {};

    if (action === "done") { metrics.termination = "done"; break; }

    if (!TOOLS_SCHEMA.some((t) => t.name === action)) {
      metrics.formatErrors++;
      messages.push({ role: "assistant", content: text });
      messages.push({ role: "user", content: `Unknown tool "${action}". Use one of: ${TOOLS_SCHEMA.map((t) => t.name).join(", ")}` });
      continue;
    }

    const t2 = performance.now();
    const injectFault = faultPending && task.fault.action === action && task.fault.path === args.path;
    const result = injectFault ? "error: temporary I/O failure; retry this operation" : executeTool(action, args, ws);
    if (injectFault) { faultPending = false; metrics.injectedFaults++; }
    metrics.trace.at(-1).toolResult = result;
    if (result.startsWith("error:")) metrics.toolErrors++;
    const t3 = performance.now();
    metrics.toolCalls++;
    metrics.toolMs += t3 - t2;
    metrics.toolLatencies.push(t3 - t2);
    metrics.trace.at(-1).toolElapsedMs = t3 - t2;

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
  const results = {};
  const report = { schemaVersion: 4, objective: "Compare end-to-end latency and independently verified task success of large and small models in the same ReAct loop", promptVersion: "relative-paths-v2", startedAt: new Date().toISOString(), node: process.version, platform: process.platform, runs: RUNS, timeoutMs: TIMEOUT, maxSteps: MAX_STEPS, maxTokens: 256, temperature: 0, toolPolicy: "workspace-files-only", configs: configs.map(({ client, ...config }) => config), warmups: [], results };
  const schedule = buildSchedule(configs, TASKS, RUNS);
  report.suite = SUITE === "expanded" ? { version: suiteDefinition.version, sha256: suiteHash, tasks: TASKS.map(({ id, category }) => ({ id, category })) } : { version: "smoke-v1" };
  report.schedule = schedule.map(({ config, task, run }) => ({ label: config.label, taskId: task.id, run }));
  mkdirSync(dirname(OUTPUT), { recursive: true });
  const save = () => {
    report.summary = Object.fromEntries(Object.entries(results).map(([label, rows]) => [label, summarizeResults(rows)]));
    writeFileSync(OUTPUT, JSON.stringify(report, null, 2));
  };
  save();

  for (const config of configs) {
    const { label, model } = config;
    results[label] = [];
    console.log(`--- ${label}: ${model} at ${config.baseURL} ---`);
    for (let i = 0; i < WARMUPS; i++) {
      const start = performance.now();
      let status = "ok";
      try {
        await config.client.chat.completions.create({ model, messages: [{ role: "user", content: 'Return JSON: {"ready":true}' }], max_tokens: 32, temperature: 0, response_format: { type: "json_object" }, ...(config.thinking === "off" ? { reasoning_effort: "none" } : {}) });
      } catch (e) { status = e.status ? `http_${e.status}` : e.constructor.name; }
      report.warmups.push({ label, elapsedMs: performance.now() - start, status });
      save();
    }
  }
  for (const [sequence, { config, task, run: i }] of schedule.entries()) {
        const { label, model } = config;
        const ws = mkdtempSync(join(tmpdir(), "jev_real_"));
        const m = newMetrics(model);
        m.taskId = task.id;
        m.run = i;
        m.sequence = sequence;
        m.category = task.category || "smoke";
        task.setup(ws);
        const start = performance.now();
        try {
          await runAgent(config, task, ws, m);
          m.success = task.verify(ws);
        } catch (e) {
          m.termination = "api_error";
          m.error = e.status ? `http_${e.status}` : e.constructor.name;
        } finally {
          m.totalMs = performance.now() - start;
          m.falseCompletion = m.termination === "done" && !m.success;
          m.finalFiles = Object.fromEntries(readdirSync(ws, { withFileTypes: true }).filter((f) => f.isFile()).map((f) => [f.name, readFileSync(join(ws, f.name), "utf8")]));
          results[label].push(m);
          save();
          console.log(`  [${sequence + 1}/${schedule.length}] ${label} ${task.id} #${i + 1}: ${m.success ? "PASS" : "FAIL"} ${m.totalMs.toFixed(0)}ms (${m.llmCalls} calls, ${m.termination})`);
          try { rmSync(ws, { recursive: true, force: true }); } catch {}
        }
  }

  // Summary
  console.log(`\n${"=".repeat(80)}`);
  console.log(`Summary\n`);
  console.log(`Mode  | Task              | Success | Avg Total | Avg LLM/call | LLM calls | Tool calls | Fmt Errors`);
  console.log(`------|-------------------|---------|-----------|--------------|-----------|------------|----------`);

  report.finishedAt = new Date().toISOString();
  save();
  console.log(`Raw results: ${OUTPUT}`);
  for (const { label } of configs) {
    const overall = report.summary[label].overall;
    console.log(`${label}: accuracy ${overall.successes}/${overall.attempts}, model calls ${overall.llmCalls}, tool calls ${overall.toolCalls}, mean elapsed ${overall.meanElapsedMs.toFixed(0)}ms, false completions ${overall.falseCompletions}`);
    const byTask = {};
    for (const r of results[label]) { (byTask[r.taskId] ??= []).push(r); }
    for (const [taskId, runs] of Object.entries(byTask)) {
      const avg = (k) => runs.reduce((s, r) => s + (r[k] || 0), 0) / runs.length;
      const ok = `${(100 * runs.filter((r) => r.success).length / runs.length).toFixed(1)}%`;
      const avgLlmLat = avg("llmCalls") > 0 ? (avg("llmMs") / avg("llmCalls")).toFixed(0) : "-";
      console.log(
        `${label.padEnd(5)} | ${taskId.padEnd(17)} | ${ok.padStart(7)} | ${(avg("totalMs") / 1000).toFixed(2).padStart(8)}s | ${String(avgLlmLat).padStart(12)}ms | ${avg("llmCalls").toFixed(1).padStart(9)} | ${avg("toolCalls").toFixed(1).padStart(10)} | ${avg("formatErrors").toFixed(1).padStart(10)}`
      );
    }
  }
}

await main();
