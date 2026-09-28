import { readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { isDeepStrictEqual } from "node:util";

const source = readFileSync(new URL("./benchmarks/tasks-v1.json", import.meta.url));
export const suiteDefinition = JSON.parse(source);
export const suiteHash = createHash("sha256").update(source).digest("hex");
const normalize = (text) => text.replaceAll("\r\n", "\n").replace(/\n$/, "");

export function writeFixture(ws, files) {
  for (const [path, value] of Object.entries(files)) {
    writeFileSync(join(ws, path), typeof value === "string" ? value : JSON.stringify(value));
  }
}

export function verifyFixture(ws, expected) {
  try {
    if (!isDeepStrictEqual(readdirSync(ws).sort(), Object.keys(expected).sort())) return false;
    return Object.entries(expected).every(([path, value]) => {
      const actual = readFileSync(join(ws, path), "utf8");
      return typeof value === "string" ? normalize(actual) === normalize(value) : isDeepStrictEqual(JSON.parse(actual), value);
    });
  } catch { return false; }
}

export const expandedTasks = suiteDefinition.tasks.map(({ expected, initial, ...task }) => ({
  ...task,
  setup: (ws) => writeFixture(ws, initial),
  verify: (ws) => verifyFixture(ws, expected),
}));

export function buildSchedule(configs, tasks, runs) {
  const schedule = [];
  for (let run = 0; run < runs; run++) {
    for (const [index, task] of tasks.entries()) {
      const ordered = (run + index) % 2 ? [...configs].reverse() : configs;
      for (const config of ordered) schedule.push({ config, task, run });
    }
  }
  return schedule;
}
