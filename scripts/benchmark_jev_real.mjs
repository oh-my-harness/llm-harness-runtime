#!/usr/bin/env node
"use strict";

/**
 * Proper benchmark: small model with JSON mode (Jev-style constrained decisions)
 * vs large model (72B) — real API calls, real latency, real success rate.
 *
 * Independent endpoints, identical prompts/tools, and recorded warmup timings.
 *
 * Usage:
 *   node scripts/benchmark_jev_real.mjs --api-key sk-... [--runs 3]
 */

import { OpenAI } from "openai";
import { mkdtempSync, rmSync, writeFileSync, readFileSync, readdirSync, mkdirSync, statSync } from "node:fs";
import { nextObservation } from "./benchmark_rules.mjs";
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
const POLICY = arg("policy", "react");
if (!["react", "rules"].includes(POLICY)) throw new Error("Invalid --policy");
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

const TASKS = [
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
  return { model, totalMs: 0, llmCalls: 0, llmMs: 0, llmLatencies: [], toolCalls: 0, toolMs: 0, steps: 0, formatErrors: 0, toolErrors: 0, success: false, taskId: "", run: 0, termination: "max_steps", trace: [] };
}

async function runAgent(config, task, ws, metrics) {
  const system = buildSystemPrompt();
  const messages = [
    { role: "system", content: system },
    { role: "user", content: `Task: ${task.prompt}` },
  ];
  const state = { initialized: false, pending: [], files: [] };
  const observe = () => {
    const start = performance.now();
    if (!state.initialized) {
      state.files = readdirSync(ws, { withFileTypes: true }).filter((f) => f.isFile()).map((f) => ({ path: f.name, size: statSync(join(ws, f.name)).size }));
    }
    let observation;
    while ((observation = nextObservation(state))) {
      const toolStart = performance.now();
      const result = executeTool(observation.action, observation.args, ws);
      const elapsedMs = performance.now() - toolStart;
      metrics.toolCalls++;
      metrics.ruleActions++;
      metrics.toolMs += elapsedMs;
      if (result.startsWith("error:")) metrics.toolErrors++;
      metrics.trace.push({ source: "rule", ...observation, toolResult: result, elapsedMs });
      messages.push({ role: "user", content: `Runtime observation (${observation.reason}, ${JSON.stringify(observation.args)}):\n${result}` });
    }
    metrics.ruleMs += performance.now() - start;
  };
  metrics.ruleActions = 0;
  metrics.ruleMs = 0;
  if (POLICY === "rules") observe();

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
    const result = executeTool(action, args, ws);
    metrics.trace.at(-1).toolResult = result;
    if (result.startsWith("error:")) metrics.toolErrors++;
    const t3 = performance.now();
    metrics.toolCalls++;
    metrics.toolMs += t3 - t2;

    messages.push({ role: "assistant", content: text });
    messages.push({ role: "user", content: `Tool result:\n${result}` });
    if (POLICY === "rules" && action === "write_file" && !result.startsWith("error:")) {
      state.pending.push({ action: "read_file", args: { path: args.path }, reason: "write_readback" });
      observe();
    }
  }
  return metrics;
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

async function main() {
  console.log(`\n=== Real API Benchmark: ${SMALL_MODEL} vs ${LARGE_MODEL} ===`);
  const results = {};
  const report = { schemaVersion: 3, promptVersion: "relative-paths-v2", policy: POLICY, ruleLimits: { files: 8, bytesPerFile: 8192 }, startedAt: new Date().toISOString(), node: process.version, platform: process.platform, runs: RUNS, timeoutMs: TIMEOUT, maxSteps: MAX_STEPS, maxTokens: 256, temperature: 0, toolPolicy: "workspace-files-only", configs: configs.map(({ client, ...config }) => config), warmups: [], results };
  mkdirSync(dirname(OUTPUT), { recursive: true });
  const save = () => writeFileSync(OUTPUT, JSON.stringify(report, null, 2));
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
    for (const task of TASKS) {
      for (let i = 0; i < RUNS; i++) {
        const ws = mkdtempSync(join(tmpdir(), "jev_real_"));
        const m = newMetrics(model);
        m.taskId = task.id;
        m.run = i;
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
          console.log(`  ${task.id} #${i + 1}: ${m.success ? "PASS" : "FAIL"} ${m.totalMs.toFixed(0)}ms (${m.llmCalls} calls, ${m.termination})`);
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

  report.finishedAt = new Date().toISOString();
  save();
  console.log(`Raw results: ${OUTPUT}`);
  for (const { label } of configs) {
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
