import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { ROOT } from '../../benchmarks/jev_browser/source.mjs';
import { approvedConfig, pairedPlan, executeMatrix, callTotals } from '../../benchmarks/jev_browser/session.mjs';
import { QwenClient, QWEN_MODEL, HttpBudget, BudgetError } from '../../benchmarks/jev_browser/qwen.mjs';
import { Trace } from '../../benchmarks/jev_browser/metrics.mjs';

const manifest = () => JSON.parse(readFileSync(resolve(ROOT, 'configs/research/jev-browser-preparation.json')));
test('approved configuration cannot silently change the sample, model or budgets', () => {
  assert.equal(approvedConfig(manifest()).laya.selection_status, 'approved');
  for (const change of [m => { m.live_run.enabled = false; },
    m => { m.development_proposal.timeout_seconds = 600; },
    m => { m.development_proposal.laya.max_len = 512; },
    m => { m.development_proposal.qwen.max_attempts = 3; }]) {
    const m = manifest(); change(m); assert.throws(() => approvedConfig(m));
  }
});
test('paired scheduling alternates order, preserves failures and separates unstarted tasks', async () => {
  const plan = pairedPlan([{ name: 'one' }, { name: 'two' }, { name: 'three' }]);
  assert.deepEqual(plan.map(p => p.arm), ['A', 'B', 'B', 'A', 'A', 'B']);
  const saved = [];
  const result = await executeMatrix(plan, {
    stopReason: () => saved.length === 3 ? 'global_http_budget' : null,
    runOne: async row => ({ ...row, success: false, termination: 'input_rejected' }),
    persist: row => saved.push(row),
  });
  assert.equal(result.results.length, 3); assert.equal(saved.length, 3);
  assert.equal(result.unstarted.length, 3);
  assert.ok(result.results.every(r => r.success === false));
  assert.ok(result.unstarted.every(r => !Object.hasOwn(r, 'success')));
});
test('global budget reserves before dispatch and includes provider retries across clients', async () => {
  const config = { model: QWEN_MODEL, endpoint: 'https://api.siliconflow.cn/v1/chat/completions',
    max_attempts: 2, timeout_ms: 1000, temperature: 0, max_tokens: 1024 };
  const budget = new HttpBudget(2), trace = new Trace();
  let dispatched = 0;
  const client = () => new QwenClient(config, trace, { apiKey: 'offline-placeholder', budget,
    fetchImpl: async () => { dispatched++; return new Response('', { status: 503 }); } });
  await assert.rejects(client().json('generation', 'test', {}));
  assert.equal(budget.used, 2); assert.equal(dispatched, 2);
  await assert.rejects(client().json('generation', 'test', {}), BudgetError);
  assert.equal(dispatched, 2); assert.equal(trace.counts().qwen_http_attempts, 2);
  assert.equal(trace.counts().qwen_retries, 1);
});
test('aborted requests do not spend global budget and unknown forward counts stay unknown', async () => {
  const budget = new HttpBudget(1), trace = new Trace();
  const c = new QwenClient({ ...approvedConfig(manifest()).qwen }, trace, {
    apiKey: 'offline-placeholder', budget, fetchImpl: () => { throw new Error('Must not dispatch'); } });
  await assert.rejects(c.json('generation', '', {}, { signal: AbortSignal.abort() }));
  assert.equal(budget.used, 0);
  assert.equal(callTotals([{ counts: { laya_forward_attempts: null } }]).laya_forward_attempts, null);
});
