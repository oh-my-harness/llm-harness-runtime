import test from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { summarizeResults } from "../benchmark_metrics.mjs";

async function fixture(t, handler) {
  const server = createServer(async (req, res) => {
    let body = "";
    for await (const chunk of req) body += chunk;
    handler(JSON.parse(body), req, res);
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => { server.closeAllConnections(); server.close(); });
  return `http://127.0.0.1:${server.address().port}/v1`;
}

function reply(res, decision) {
  res.writeHead(200, { "Content-Type": "application/json" });
  res.end(JSON.stringify({ choices: [{ message: { content: JSON.stringify(decision) }, finish_reason: "stop" }] }));
}

async function run(t, args) {
  const dir = mkdtempSync(join(tmpdir(), "jev-test-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const output = join(dir, "results.json");
  const child = spawn(process.execPath, ["scripts/benchmark_jev_real.mjs", "--runs", "1", "--warmups", "0", "--output", output, ...args], {
    env: { ...process.env, SF_API_KEY: "", LARGE_API_KEY: "large-test-secret", SMALL_API_KEY: "small-test-secret" },
  });
  let stderr = "";
  child.stderr.on("data", (data) => { stderr += data; });
  child.stdout.resume();
  const code = await new Promise((resolve, reject) => { child.on("error", reject); child.on("exit", resolve); });
  assert.equal(code, 0, stderr);
  const raw = readFileSync(output, "utf8");
  assert.ok(!raw.includes("test-secret"));
  return JSON.parse(raw);
}

test("independent endpoints preserve API failure latency and successful file tasks", async (t) => {
  let failures = 0;
  const large = await fixture(t, (_body, req, res) => {
    assert.equal(req.headers.authorization, "Bearer large-test-secret");
    failures++;
    setTimeout(() => { res.writeHead(503); res.end('{}'); }, 30);
  });
  const small = await fixture(t, (body, req, res) => {
    assert.equal(req.headers.authorization, "Bearer small-test-secret");
    assert.equal(body.reasoning_effort, "none");
    const messages = body.messages;
    if (messages.length > 2) return reply(res, { action: "done" });
    const prompt = messages[1].content;
    assert.ok(!prompt.includes("Workspace:"));
    const [path, content] = prompt.includes("rename")
      ? ["app.py", "def new_name(x):\n    return x * 2\n\nprint(new_name(5))"]
      : prompt.includes("comment") ? ["config.py", "# TODO: review\r\nDEBUG = True\r\n"] : ["README.md", "Hello World"];
    reply(res, { action: "write_file", args: { path, content } });
  });
  const report = await run(t, ["--large-base-url", large, "--small-base-url", small, "--small-thinking", "off"]);
  assert.equal(failures, 3, "SDK retries must be disabled");
  for (const row of report.results.large) {
    assert.equal(row.error, "http_503");
    assert.equal(row.success, false);
    assert.equal(row.llmCalls, 1);
    assert.ok(row.totalMs >= 25);
    assert.ok(row.llmMs >= 25);
  }
  assert.ok(report.results.small.every((row) => row.success && row.termination === "done"));
  assert.equal(report.summary.small.overall.accuracy, 1);
  assert.equal(report.summary.large.overall.accuracy, 0);
  assert.equal(report.summary.large.overall.apiFailures, 3);
  assert.equal(report.summary.small.overall.llmCalls, 6);
  assert.ok(report.results.small.every((row) => row.toolLatencies.length === row.toolCalls));
});

test("invalid decision JSON and out-of-workspace paths are recorded without crashing", async (t) => {
  const endpoint = await fixture(t, (body, _req, res) => {
    const count = body.messages.length;
    if (count === 2) return reply(res, null);
    if (count === 4) return reply(res, { action: "write_file", args: { path: "../escaped.txt", content: "bad" } });
    reply(res, { action: "done" });
  });
  const report = await run(t, ["--mode", "small", "--small-base-url", endpoint]);
  for (const row of report.results.small) {
    assert.equal(row.formatErrors, 1);
    assert.equal(row.toolErrors, 1);
    assert.equal(row.success, false);
    assert.match(row.trace[1].toolResult, /outside workspace/);
  }
});

test("timeouts retain elapsed call time and do not trigger hidden retries", async (t) => {
  const endpoint = await fixture(t, () => {});
  const report = await run(t, ["--mode", "small", "--small-base-url", endpoint, "--timeout-ms", "50"]);
  for (const row of report.results.small) {
    assert.equal(row.termination, "api_error");
    assert.equal(row.llmCalls, 1);
    assert.ok(row.llmMs >= 40);
    assert.ok(row.totalMs >= row.llmMs);
  }
});

test("adding a comment must not erase the existing configuration", async (t) => {
  const endpoint = await fixture(t, (body, _req, res) => {
    reply(res, body.messages.length === 2
      ? { action: "write_file", args: { path: "config.py", content: "# TODO: review\n" } }
      : { action: "done" });
  });
  const report = await run(t, ["--mode", "small", "--small-base-url", endpoint]);
  assert.equal(report.results.small.find((r) => r.taskId === "add_comment").success, false);
  assert.equal(report.results.small.find((r) => r.taskId === "add_comment").falseCompletion, true);
});

test("summary includes failures and uses weighted per-call latency", () => {
  const rows = [
    { taskId: "a", success: true, termination: "done", totalMs: 100, llmMs: 80, llmCalls: 2, toolMs: 10, toolCalls: 1 },
    { taskId: "a", success: false, termination: "done", totalMs: 300, llmMs: 270, llmCalls: 1, toolMs: 20, toolCalls: 2 },
    { taskId: "b", success: false, termination: "api_error", totalMs: 600, llmMs: 600, llmCalls: 1, toolMs: 0, toolCalls: 0 },
  ];
  const result = summarizeResults(rows);
  assert.equal(result.overall.accuracy, 1 / 3);
  assert.equal(result.overall.totalElapsedMs, 1000);
  assert.equal(result.overall.meanSuccessfulElapsedMs, 100);
  assert.equal(result.overall.meanFailedElapsedMs, 450);
  assert.equal(result.overall.meanLlmCallMs, 950 / 4);
  assert.equal(result.overall.medianElapsedMs, 300);
  assert.equal(result.overall.p95ElapsedMs, 600);
  assert.equal(result.overall.falseCompletions, 1);
  assert.equal(result.byTask.a.accuracy, 0.5);
  assert.equal(result.byTask.a.medianElapsedMs, 200);
  assert.equal(result.byTask.b.meanSuccessfulElapsedMs, null);
  assert.equal(summarizeResults([]).overall.accuracy, null);
});
