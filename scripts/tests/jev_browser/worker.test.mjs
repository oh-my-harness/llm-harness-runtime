import test from 'node:test';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { ROOT } from '../../benchmarks/jev_browser/source.mjs';
import { LayaWorker } from '../../benchmarks/jev_browser/laya.mjs';
import { Trace } from '../../benchmarks/jev_browser/metrics.mjs';

const fixture = fileURLToPath(new URL('./worker-fixture.mjs', import.meta.url));
const config = { selection_status: 'approved', load_timeout_ms: 5000, request_timeout_ms: 1000 };
const worker = () => new LayaWorker(config, { python: process.execPath, script: fixture });
const body = state => ({ model: 'jev-latest', state, questions: { operation: {} } });

test('persistent worker separates model setup and task forwards, filters credentials', async () => {
  const original = process.env.JEV_TEST_API_KEY;
  process.env.JEV_TEST_API_KEY = 'test-placeholder';
  const w = worker(), trace = new Trace();
  try {
    await w.start(trace);
    assert.equal(trace.events[0].component, 'model_setup');
    assert.equal(trace.events[0].setup.secret_present, false);
    await w.predict(body('first'), trace);
    await w.predict(body('second'), trace);
    assert.equal(trace.counts().laya_predict_calls, 2);
    assert.equal(trace.counts().laya_forward_attempts, 2);
    assert.equal(trace.counts().laya_observed_forward_starts, 2);
  } finally {
    if (original === undefined) delete process.env.JEV_TEST_API_KEY;
    else process.env.JEV_TEST_API_KEY = original;
    await w.close();
  }
});
for (const state of ['hang', 'exit', 'bad-id']) {
  test(`worker ${state} cannot turn incomplete forward accounting into zero`, async () => {
    const w = worker(), trace = new Trace();
    try {
      await w.start(trace);
      await assert.rejects(w.predict(body(state), trace));
      assert.equal(trace.counts().laya_forward_attempts, null);
      assert.equal(trace.counts().laya_observed_forward_starts, 1);
      await w.start(trace);
      await w.predict(body('restarted'), trace);
      assert.equal(trace.counts().laya_rpc_attempts, 2);
    } finally { await w.close(); }
  });
}
test('predict cannot lazily load a model inside task timing', async () => {
  const w = worker();
  await assert.rejects(w.predict(body('not-ready'), new Trace()));
  assert.equal(w.child, null);
});

test('real Python IPC preserves all frozen input characters and verifies their wire hash', async () => {
  const script = fileURLToPath(new URL('./worker-python-fixture.py', import.meta.url));
  const w = new LayaWorker(config, { script }), trace = new Trace();
  const file = resolve(ROOT, 'logs/benchmarks/setup/jev-browser-input-audit-20260929-02/captures.json');
  const inputs = [{ model: 'test', state: { text: '中文 ♦ → café 😀 \n a  b' }, questions: {} }];
  try { inputs.push(...JSON.parse(readFileSync(file)).rows.map(r => r.request)); }
  catch { /* The multilingual regression is portable without local captures. */ }
  try {
    await w.start(trace);
    assert.match(trace.events[0].setup.stdin_encoding, /^utf-?8$/i);
    for (const input of inputs) {
      const result = await w.predict(input, trace);
      assert.deepEqual(result.answers.echo, input.state);
      assert.equal(result.answers.sha256, createHash('sha256').update(JSON.stringify(input.state)).digest('hex'));
      assert.equal(trace.events.at(-1).input_transport_verified, true);
    }
  } finally { await w.close(); }
});

test('legacy GBK pipe reproduction is detected without loading a model', () => {
  const w = new LayaWorker(config);
  const input = JSON.stringify({ text: '中文 ♦ → café 😀' });
  const source = 'import sys,json; print(json.dumps(sys.stdin.read()))';
  const env = { ...process.env, PYTHONIOENCODING: 'gbk:surrogateescape', PYTHONUTF8: '0' };
  const child = spawnSync(w.python, ['-c', source], { env, input, encoding: 'utf8', windowsHide: true });
  assert.equal(child.status, 0);
  assert.notEqual(JSON.parse(child.stdout), input);
});
