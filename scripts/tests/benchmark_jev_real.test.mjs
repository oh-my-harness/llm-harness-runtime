import test from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { nextObservation } from "../benchmark_rules.mjs";

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

test("rules provide observations but cannot claim completion from a successful write", async (t) => {
  const endpoint = await fixture(t, (body, _req, res) => {
    const observations = body.messages.filter((m) => m.content.startsWith("Runtime observation"));
    assert.ok(observations.some((m) => m.content.includes("initial_inventory")));
    if (observations.some((m) => m.content.includes("write_readback"))) {
      return reply(res, { action: "done" });
    }
    reply(res, { action: "write_file", args: { path: "config.py", content: "# TODO: review\n" } });
  });
  const report = await run(t, ["--mode", "large", "--large-base-url", endpoint, "--policy", "rules"]);
  for (const row of report.results.large) {
    assert.equal(row.llmCalls, 2, "completion must still be a model decision");
    assert.equal(row.success, false);
    assert.equal(row.falseCompletion, true);
    assert.ok(row.trace.some((event) => event.source === "rule" && event.reason === "write_readback"));
  }
});

test("generic observation scheduler bounds reads and exhausts without a done action", () => {
  const state = { initialized: false, pending: [], files: [
    { path: "too-large", size: 8193 },
    ...Array.from({ length: 12 }, (_, i) => ({ path: `file-${i}`, size: 10 })),
  ] };
  const actions = [];
  let action;
  while ((action = nextObservation(state))) actions.push(action);
  assert.equal(actions.length, 9);
  assert.equal(actions[0].action, "list_files");
  assert.ok(!actions.some((a) => a.action === "done" || a.args.path === "too-large"));
});
