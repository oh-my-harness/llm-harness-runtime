import { mkdirSync, writeFileSync, appendFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { resolve, relative, isAbsolute } from 'node:path';
import { pathToFileURL } from 'node:url';
import { ROOT, loadSource } from './source.mjs';

export const digest = value => createHash('sha256').update(JSON.stringify(value)).digest('hex');

export async function captureTask(upstream, browser, task, port, foreignPort, timeoutMs = 45000) {
  const row = { task_id: task.name, phase: 'initial_only', started_utc: new Date().toISOString(),
    requested_url: task.url(port, foreignPort), model_calls: 0, model_executed_actions: 0 };
  const start = performance.now();
  let context, timer, work;
  try {
    context = await browser.newContext({ viewport: { width: 1120, height: 780 },
      acceptDownloads: false, serviceWorkers: 'block' });
    context.setDefaultTimeout(15000);
    const page = await context.newPage();
    work = (async () => {
      const response = await page.goto(row.requested_url, { waitUntil: 'load', timeout: 30000 });
      row.http_status = response?.status() ?? null;
      row.final_url = page.url();
      if (response && !response.ok()) { row.status = 'http_error'; return; }
      try { await page.waitForLoadState('networkidle', { timeout: 5000 }); }
      catch { row.network_idle_reached = false; }
      const observation = await upstream.observe(context, { screenshot: false, diagnostics: false });
      row.title = observation.title;
      row.observation_fingerprint = observation.fingerprint;
      row.page_text_chars = observation.text.length;
      row.actions = observation.actions.length;
      row.request = upstream.buildDecisionRequest(observation, task.goal, []).body;
      row.request_sha256 = digest(row.request);
      row.status = 'captured';
    })();
    await Promise.race([work, new Promise((_, reject) => {
      timer = setTimeout(() => { void context.close().catch(() => {}); reject(new Error('capture_deadline')); }, timeoutMs);
    })]);
  } catch (error) {
    row.status = 'capture_error';
    row.error_type = error.name;
    row.network_error = /net::[A-Z_]+/.exec(error.message)?.[0] ?? null;
  } finally {
    clearTimeout(timer);
    await context?.close().catch(() => {});
    await work?.catch(() => {});
    row.elapsed_ms = performance.now() - start;
  }
  return row;
}

export async function capture(output) {
  const base = resolve(ROOT, 'logs/benchmarks/setup');
  output = resolve(output);
  const subpath = relative(base, output);
  if (!subpath || subpath.startsWith('..') || isAbsolute(subpath))
    throw new Error('Output must be a new directory under logs/benchmarks/setup');
  mkdirSync(output);
  const upstream = await loadSource();
  const originalFetch = globalThis.fetch;
  globalThis.fetch = () => { throw new Error('Model fetch is forbidden during input capture'); };
  let browser, local, foreign;
  const report = { schema_version: 1, scope: 'initial_input_capture_not_model_evaluation',
    started_utc: new Date().toISOString(), upstream_revision: upstream.revision,
    node: process.version, platform: process.platform, viewport: { width: 1120, height: 780 },
    task_ids: upstream.config.task_set.ids, model_calls: 0, model_executed_actions: 0, rows: [] };
  const events = resolve(output, 'capture-events.jsonl');
  writeFileSync(events, JSON.stringify({ kind: 'manifest', ...report }) + '\n', { flag: 'wx' });
  try {
    local = await upstream.fixtures.start(0);
    foreign = await upstream.fixtures.start(0);
    browser = await upstream.chromium.launch({ headless: true });
    report.browser = browser.version();
    for (const task of upstream.journeys) {
      const row = await captureTask(upstream, browser, task, local.port, foreign.port);
      report.rows.push(row);
      appendFileSync(events, JSON.stringify({ kind: 'capture', ...row }) + '\n');
      console.log(JSON.stringify({ task: row.task_id, status: row.status, actions: row.actions,
        http_status: row.http_status, network_error: row.network_error }));
    }
  } finally {
    await browser?.close();
    if (local) await upstream.fixtures.stop(local.server);
    if (foreign) await upstream.fixtures.stop(foreign.server);
    globalThis.fetch = originalFetch;
    writeFileSync(resolve(output, 'captures.json'), JSON.stringify(report, null, 2), { flag: 'wx' });
  }
  return report;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  if (process.argv.length !== 4 || process.argv[2] !== '--output') throw new Error('Expected --output NEW_DIRECTORY');
  await capture(process.argv[3]);
}
