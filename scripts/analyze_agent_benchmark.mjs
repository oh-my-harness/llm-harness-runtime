import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { createHash } from "node:crypto";
import { summarizeResults } from "./benchmark_metrics.mjs";

const [input, output] = process.argv.slice(2);
if (!input || !output) throw new Error("Usage: node scripts/analyze_agent_benchmark.mjs INPUT.json OUTPUT.json");
const raw = readFileSync(input);
const report = JSON.parse(raw);
if (!report.finishedAt) throw new Error("Refusing to summarize an unfinished run as final");
const counts = Object.values(report.results).reduce((sum, rows) => sum + rows.length, 0);
if (counts !== report.schedule.length) throw new Error("Measured attempts do not match the recorded schedule");

const mean = (values) => values.length ? values.reduce((sum, n) => sum + n, 0) / values.length : null;
const paired = [];
for (const large of report.results.large || []) {
  const small = report.results.small?.find((row) => row.taskId === large.taskId && row.run === large.run);
  if (small) paired.push({ taskId: large.taskId, run: large.run, largeSuccess: large.success, smallSuccess: small.success, largeMs: large.totalMs, smallMs: small.totalMs });
}
const joint = paired.filter((pair) => pair.largeSuccess && pair.smallSuccess);
const summary = Object.fromEntries(Object.entries(report.results).map(([label, rows]) => [label, {
  ...summarizeResults(rows),
  byCategory: Object.fromEntries(Object.entries(Object.groupBy(rows, (row) => row.category)).map(([category, group]) => [category, summarizeResults(group).overall])),
  failureSignals: {
    failedWithApiError: rows.filter((row) => !row.success && row.termination === "api_error").length,
    failedWithToolError: rows.filter((row) => !row.success && row.toolErrors > 0).length,
    failedWithFormatError: rows.filter((row) => !row.success && row.formatErrors > 0).length,
    failedAtStepLimit: rows.filter((row) => !row.success && row.termination === "max_steps").length,
    injectedFaults: rows.reduce((sum, row) => sum + row.injectedFaults, 0),
  },
}]));
const artifact = {
  source: input.replaceAll("\\", "/"),
  sourceSha256: createHash("sha256").update(raw).digest("hex"),
  suite: report.suite,
  startedAt: report.startedAt,
  finishedAt: report.finishedAt,
  promptVersion: report.promptVersion,
  configs: report.configs,
  limits: { maxSteps: report.maxSteps, maxTokens: report.maxTokens, timeoutMs: report.timeoutMs, temperature: report.temperature },
  warmups: report.warmups,
  summary,
  paired: {
    attempts: paired.length,
    bothPassed: joint.length,
    onlyLargePassed: paired.filter((pair) => pair.largeSuccess && !pair.smallSuccess).length,
    onlySmallPassed: paired.filter((pair) => !pair.largeSuccess && pair.smallSuccess).length,
    bothFailed: paired.filter((pair) => !pair.largeSuccess && !pair.smallSuccess).length,
    meanLargeMsOnJointSuccess: mean(joint.map((pair) => pair.largeMs)),
    meanSmallMsOnJointSuccess: mean(joint.map((pair) => pair.smallMs)),
    jointSuccessTimeRatio: joint.length ? mean(joint.map((pair) => pair.largeMs)) / mean(joint.map((pair) => pair.smallMs)) : null,
  },
  attempts: Object.fromEntries(Object.entries(report.results).map(([label, rows]) => [label, rows.map(({ trace, finalFiles, ...row }) => ({ ...row, ...(!row.success ? { finalFiles } : {}) }))])),
};
mkdirSync(dirname(resolve(output)), { recursive: true });
writeFileSync(output, JSON.stringify(artifact, null, 2));
console.log(JSON.stringify({ overall: Object.fromEntries(Object.entries(summary).map(([label, value]) => [label, value.overall])), paired: artifact.paired }, null, 2));
