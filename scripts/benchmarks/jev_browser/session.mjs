import { mkdirSync, readFileSync, writeFileSync, appendFileSync, readdirSync } from 'node:fs';
import { resolve, relative, isAbsolute } from 'node:path';
import { pathToFileURL } from 'node:url';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import os from 'node:os';
import { ROOT, loadSource } from './source.mjs';
import { QwenClient, QWEN_MODEL, HttpBudget, textValue } from './qwen.mjs';
import { LayaWorker } from './laya.mjs';
import { Trace, summarize } from './metrics.mjs';
import { makePolicy } from './policy.mjs';
import { runAttempt } from './attempt.mjs';
import { digest } from './capture-inputs.mjs';

const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const CAPTURES = 'logs/benchmarks/setup/jev-browser-input-audit-20260929-02/captures.json';
const CAPTURE_HASH = '2492b25ee705af6760aa71d3a85c4297de4035b8813e89bda4c27a506dfc2512';
const endpoint = 'https://api.siliconflow.cn/v1/chat/completions';

export function approvedConfig(manifest) {
  const p = manifest.development_proposal;
  if (!manifest.live_run.enabled || p.approval !== 'confirmed_by_lqj_2026-09-29_start')
    throw new Error('Missing explicit run approval');
  const expected = { repetitions: 1, concurrency: 1, order: 'alternate_AB_BA_by_task_index',
    timeout_seconds: 180, max_actions: 30, max_decisions: 60, browser_timeout_ms: 15000,
    session_timeout_seconds: 3600, qwen_max_http_attempts_total_including_preflight: 120 };
  for (const [k, v] of Object.entries(expected)) if (p[k] !== v) throw new Error(`Unapproved ${k}`);
  const laya = { model: 'convaiinnovations/laya', revision: '55cf4c4ebb4ebe31b2550e8bdf3bd21b99753851',
    code_revision: '9d955671415fc19f069b9cc998928075c1f255ec', state_encoding: 'lossless_compact_json_string',
    max_len: 8192, head_max_len: 4096, precision: 'bf16', load_timeout_ms: 180000,
    request_timeout_ms: 45000, warmup_calls: 1, preflight_max_initial_states: 7, automatic_fallback: false };
  const qwen = { temperature: 0, max_tokens: 1024, timeout_ms: 45000, max_attempts: 2, preflight_max_http_attempts: 2 };
  for (const [group, expectedGroup] of Object.entries({ laya, qwen }))
    for (const [k, v] of Object.entries(expectedGroup))
      if (p[group][k] !== v) throw new Error(`Unapproved ${group}.${k}`);
  return { ...p, laya: { ...laya, selection_status: 'approved' }, qwen: { ...qwen, model: QWEN_MODEL, endpoint } };
}

export function pairedPlan(tasks) {
  return tasks.flatMap((task, i) => (i % 2 ? ['B', 'A'] : ['A', 'B'])
    .map(arm => ({ task_id: task.name, arm, repetition: 0 })));
}

export async function executeMatrix(plan, { runOne, stopReason, persist }) {
  const results = [];
  for (let i = 0; i < plan.length; i++) {
    const reason = stopReason();
    if (reason) return { results, unstarted: plan.slice(i).map(p => ({ ...p, reason })) };
    const row = await runOne(plan[i], i);
    results.push(row);
    await persist(row, i);
  }
  return { results, unstarted: [] };
}

export function callTotals(rows) {
  const keys = ['qwen_http_attempts', 'qwen_logical_requests', 'qwen_retries',
    'laya_rpc_attempts', 'laya_predict_calls', 'laya_forward_attempts',
    'laya_observed_forward_starts', 'execution_attempts'];
  return Object.fromEntries(keys.map(k => [k, rows.every(r => Number.isFinite(r.counts[k]))
    ? rows.reduce((n, r) => n + r.counts[k], 0) : null]));
}

function command(program, args) {
  try { return execFileSync(program, args, { cwd: ROOT, encoding: 'utf8', windowsHide: true }).trim(); }
  catch { return null; }
}

export async function runSession(output) {
  const started = performance.now();
  const base = resolve(ROOT, 'logs/benchmarks'), subpath = relative(base, resolve(output));
  if (!subpath || subpath.startsWith('..') || isAbsolute(subpath)) throw new Error('Output must be new and under logs/benchmarks');
  const upstream = await loadSource(), config = approvedConfig(upstream.config);
  // Never accept credentials from argv or config, and never print provider exception messages.
  if (!process.env.SILICONFLOW_API_KEY) throw new Error('SILICONFLOW_API_KEY is missing');
  const bytes = readFileSync(resolve(ROOT, CAPTURES));
  if (hash(bytes) !== CAPTURE_HASH) throw new Error('Frozen preflight input changed');
  const captures = JSON.parse(bytes).rows;
  if (JSON.stringify(captures.map(r => r.task_id)) !== JSON.stringify(upstream.config.task_set.ids)
    || captures.some(r => r.status !== 'captured' || r.request_sha256 !== digest(r.request)))
    throw new Error('Invalid preflight input manifest');
  output = resolve(output); mkdirSync(output);
  const save = (name, value) => writeFileSync(resolve(output, name), JSON.stringify(value, null, 2), { flag: 'wx' });
  const append = (name, value) => appendFileSync(resolve(output, name), JSON.stringify(value) + '\n');
  const plan = pairedPlan(upstream.journeys), budget = new HttpBudget(120);
  const trace = new Trace(resolve(output, 'setup-events.jsonl'));
  const laya = new LayaWorker(config.laya);
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(new Error('Session deadline exceeded')),
    Math.max(1, config.session_timeout_seconds * 1000 - (performance.now() - started)));
  const manifest = { schema_version: 1, scope: 'development_not_quality_equivalence',
    started_utc: new Date().toISOString(), upstream_revision: upstream.revision, config, plan,
    preflight_capture_sha256: CAPTURE_HASH, runtime_revision: command('git', ['rev-parse', 'HEAD']),
    runtime_worktree_status: command('git', ['status', '--short']),
    code_sha256: Object.fromEntries(readdirSync(resolve(ROOT, 'scripts/benchmarks/jev_browser'))
      .filter(n => /\.(mjs|py)$/.test(n)).map(n => [n, hash(readFileSync(resolve(ROOT, 'scripts/benchmarks/jev_browser', n)))])),
    pilot_models_sha256: hash(readFileSync(resolve(ROOT, 'scripts/benchmarks/pilot_models.py'))),
    host: { os: os.type(), release: os.release(), arch: os.arch(), cpu: os.cpus()[0].model,
      memory_bytes: os.totalmem(), node: process.version, viewport: { width: 1120, height: 780 },
      gpu: command('nvidia-smi', ['--query-gpu=name,driver_version,memory.total,memory.free', '--format=csv,noheader']) } };
  save('manifest.json', manifest);
  let browser, local, foreign, matrix = { results: [], unstarted: plan.map(p => ({ ...p, reason: 'preflight_not_passed' })) };
  let status = 'preflight_failed', errorType = null;
  try {
    await laya.start(trace, controller.signal);
    console.log(JSON.stringify({ phase: 'laya_loaded_and_warmed' }));
    for (const row of captures) {
      await trace.measure('preflight', row.task_id, async () => {
        const answer = await laya.predict(row.request, trace, controller.signal);
        for (const [name, question] of Object.entries(row.request.questions))
          if (!Object.hasOwn(question.criteria, answer.answers?.[name]?.choice))
            throw new Error('Invalid Laya preflight selection');
      });
      console.log(JSON.stringify({ phase: 'laya_preflight_passed', task_id: row.task_id }));
    }
    const qwen = new QwenClient(config.qwen, trace, { budget });
    await qwen.json('generation', 'Return a JSON object with exactly one field text, whose value is ready.',
      { purpose: 'connectivity_preflight_not_task_evaluation' }, { signal: controller.signal, accept: value => {
        const text = textValue(value); if (text !== 'ready') throw new Error('Preflight content mismatch'); return text;
      } });
    save('preflight.json', { passed: true, counts: trace.counts(), elapsed_ms: performance.now() - started });
    local = await upstream.fixtures.start(0); foreign = await upstream.fixtures.start(0);
    browser = await trace.measure('browser_setup', 'launch', () => upstream.chromium.launch({ headless: true }));
    save('browser.json', { version: browser.version(), local_port: local.port, foreign_port: foreign.port });
    const limits = { timeout_ms: 180000, max_actions: 30, max_decisions: 60, browser_timeout_ms: 15000 };
    status = 'running';
    matrix = await executeMatrix(plan, {
      stopReason: () => controller.signal.aborted ? 'session_timeout'
        : budget.used >= budget.limit ? 'global_http_budget' : !laya.ready ? 'laya_worker_unavailable' : null,
      runOne: async (item, index) => {
        const task = upstream.journeys.find(t => t.name === item.task_id);
        const events = new Trace(resolve(output, `${String(index + 1).padStart(2, '0')}-${task.name}-${item.arm}.jsonl`));
        try {
          const qwen = new QwenClient(config.qwen, events, { budget });
          const policy = makePolicy(item.arm, { upstream, qwen, laya, trace: events });
          const started_utc = new Date().toISOString();
          append('lifecycle.jsonl', { ...item, index, state: 'started', started_utc });
          return { ...await runAttempt({ upstream, browser, task, url: task.url(local.port, foreign.port),
            ...item, limits, policy, trace: events, signal: controller.signal }), started_utc };
        } finally { events.close(); }
      },
      persist: (row, index) => {
        append('attempts.jsonl', row);
        append('lifecycle.jsonl', { index, state: 'finished' });
        console.log(JSON.stringify({ phase: 'attempt_finished', task: row.task_id, arm: row.arm,
          success: row.success, termination: row.termination, error_type: row.error_type,
          elapsed_ms: row.elapsed_ms, qwen_http: row.counts.qwen_http_attempts, laya_forwards: row.counts.laya_forward_attempts }));
      },
    });
    status = matrix.unstarted.length ? 'stopped' : 'completed';
  } catch (error) {
    errorType = error.name;
    // Recover durable completed attempts even if orchestration itself was interrupted.
    try { matrix.results = readFileSync(resolve(output, 'attempts.jsonl'), 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse); }
    catch { /* No completed tasks before preflight failure. */ }
    const finished = new Set(matrix.results.map(r => `${r.task_id}:${r.arm}`));
    try {
      const lifecycle = readFileSync(resolve(output, 'lifecycle.jsonl'), 'utf8').trim().split('\n').map(JSON.parse);
      for (const row of lifecycle.filter(r => r.state === 'started' && !finished.has(`${r.task_id}:${r.arm}`))) {
        matrix.results.push({ task_id: row.task_id, arm: row.arm, repetition: row.repetition,
          success: false, termination: 'interrupted_unknown', started_utc: row.started_utc,
          elapsed_ms: null, total_ms: null, counts: {} });
        finished.add(`${row.task_id}:${row.arm}`);
      }
    } catch { /* No task lifecycle exists during setup. */ }
    matrix.unstarted = plan.filter(p => !finished.has(`${p.task_id}:${p.arm}`))
      .map(p => ({ ...p, reason: status === 'preflight_failed' ? 'preflight_failed' : 'session_error' }));
    if (status !== 'preflight_failed') status = 'session_error';
    console.log(JSON.stringify({ phase: status, error_type: errorType }));
  } finally {
    clearTimeout(timer);
    await trace.measure('cleanup', 'session', async () => {
      await browser?.close();
      if (local) await upstream.fixtures.stop(local.server);
      if (foreign) await upstream.fixtures.stop(foreign.server);
      await laya.close();
    }).catch(() => {});
    const both = matrix.results.filter(r => r.success && matrix.results.some(other => other.task_id === r.task_id
      && other.arm !== r.arm && other.success && other.initial?.expected_story_id === r.initial?.expected_story_id));
    const report = { status, error_type: errorType, finished_utc: new Date().toISOString(),
      wall_ms_including_setup_cleanup: performance.now() - started, actual_qwen_http_total: budget.used,
      preflight_and_setup_counts: trace.counts(), summary: summarize(matrix.results),
      calls: Object.fromEntries(['A', 'B'].map(arm => [arm, {
        all: callTotals(matrix.results.filter(r => r.arm === arm)),
        both_pass_comparable: callTotals(both.filter(r => r.arm === arm)) }])), ...matrix };
    save('report.json', report); trace.close();
    console.log(JSON.stringify({ phase: 'session_finished', status, attempts: matrix.results.length,
      unstarted: matrix.unstarted.length, qwen_http: budget.used, summary: report.summary }));
    return report;
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  if (process.argv.length !== 5 || process.argv[2] !== '--live' || process.argv[3] !== '--output')
    throw new Error('Expected --live --output NEW_DIRECTORY');
  const report = await runSession(process.argv[4]);
  if (report.status !== 'completed') process.exitCode = 1;
}
