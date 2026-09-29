import test from 'node:test';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
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
