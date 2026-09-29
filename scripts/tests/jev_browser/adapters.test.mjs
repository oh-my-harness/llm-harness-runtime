import test, { before, after } from 'node:test';
import assert from 'node:assert/strict';
import { setTimeout as sleep } from 'node:timers/promises';
import { mkdtempSync, readFileSync, rmSync, rmdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { loadSource } from '../../benchmarks/jev_browser/source.mjs';
import { Trace, summarize } from '../../benchmarks/jev_browser/metrics.mjs';
import { QwenClient, QWEN_MODEL, textValue } from '../../benchmarks/jev_browser/qwen.mjs';
import { makePolicy, fieldContext } from '../../benchmarks/jev_browser/policy.mjs';
import { prepareAcceptance } from '../../benchmarks/jev_browser/acceptance.mjs';
import { runAttempt } from '../../benchmarks/jev_browser/attempt.mjs';

const config = { model: QWEN_MODEL, endpoint: 'https://api.siliconflow.cn/v1/chat/completions',
  temperature: 0, max_tokens: 256, max_attempts: 2, timeout_ms: 5000 };
let upstream, browser, local, foreign;
const originalFetch = globalThis.fetch;
before(async () => {
  globalThis.fetch = () => { throw new Error('Real model/network fetch forbidden in offline tests'); };
  upstream = await loadSource();
  local = await upstream.fixtures.start(0);
  foreign = await upstream.fixtures.start(0);
  browser = await upstream.chromium.launch({ headless: true });
});
after(async () => {
  await browser?.close();
  if (local) await upstream.fixtures.stop(local.server);
  if (foreign) await upstream.fixtures.stop(foreign.server);
  globalThis.fetch = originalFetch;
});

function response(answer) {
  return Response.json({ model: QWEN_MODEL, choices: [{ message: { content: JSON.stringify(answer) }, finish_reason: 'stop' }],
    usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 } });
}
function fakeQwen(trace, choose, options = {}) {
  return new QwenClient({ ...config, ...options }, trace, { apiKey: 'offline-test-placeholder',
    fetchImpl: async (_url, request) => {
      assert.equal(request.redirect, 'error');
      const body = JSON.parse(request.body);
      return response(choose(JSON.parse(body.messages[1].content)));
    } });
}
function choice(criteria, selected) {
  return { type: 'choice', choice: selected, confidence: 1,
    probabilities: Object.fromEntries(Object.keys(criteria).map(k => [k, +(k === selected)])) };
}
function fakeLaya(choose) {
  return { predict: (body, trace) => trace.measure('laya', 'decision', async () => {
    const selected = choose(body), answers = { operation: choice(body.questions.operation.criteria, selected.operation) };
    const head = selected.operation.toLowerCase() + '_target';
    if (body.questions[head]) answers[head] = choice(body.questions[head].criteria, selected.target);
    return { answers, usage: { input_tokens: 1, output_tokens: 1 } };
  }, { actual_calls: 1, forward_attempts: 1, forward_starts: 1, forward_count_complete: true,
    decision_questions: Object.keys(body.questions).length, test_double: true }) };
}
function observation() {
  return { title: 'Test', url: 'http://localhost/', text: 'Email form', tabs: [],
    actions: [{ id: 'fill-email', kind: 'fill', label: 'Email', role: 'textbox', currentValue: '',
      ref: { frameId: 'main', node: 'email' } },
    { id: 'click-submit', kind: 'click', label: 'Submit', role: 'button', ref: { frameId: 'main', node: 'submit' } },
    { id: 'wait', kind: 'wait', label: 'Wait' }] };
}

test('source is the approved seven journeys, live runs remain disabled', () => {
  assert.equal(upstream.journeys.length, 7);
  assert.equal(upstream.config.live_run.enabled, false);
});
test('A combines selection and field text in one real transport attempt', async () => {
  const trace = new Trace(), seen = observation();
  const policy = makePolicy('A', { upstream, trace, qwen: fakeQwen(trace,
    () => ({ operation: 'TYPE_TEXT', target: '1', text: 'ada@example.com' })) });
  const selected = await policy.decide(seen, 'Fill email', [], new Map());
  assert.equal(selected.choice, 'fill-email');
  const text = await policy.fieldText(fieldContext(seen, 'Fill email', [], seen.actions[0]));
  assert.equal(text.value, 'ada@example.com');
  assert.equal(trace.counts().qwen_http_attempts, 1);
  assert.equal(trace.counts().qwen_by_purpose.mixed, 1);
  await assert.rejects(policy.fieldText({ goal: 'Unrelated context' }));
});
test('B selects the field via Laya and generates only for that selected field', async () => {
  const trace = new Trace(), seen = observation();
  const policy = makePolicy('B', { upstream, trace, laya: fakeLaya(() => ({ operation: 'TYPE_TEXT', target: '1' })),
    qwen: fakeQwen(trace, input => { assert.equal(input.field.label, 'Email'); return { text: 'ada@example.com' }; }) });
  const selected = await policy.decide(seen, 'Fill email', [], new Map());
  assert.equal(selected.choice, 'fill-email');
  assert.equal(trace.counts().qwen_http_attempts, 0);
  await policy.fieldText(fieldContext(seen, 'Fill email', [], seen.actions[0]));
  assert.equal(trace.counts().qwen_by_purpose.generation, 1);
  assert.equal(trace.counts().laya_predict_calls, 1);
});
for (const arm of ['A', 'B']) {
  test(`${arm}: target cache is reused, control decisions are not cached`, async () => {
    const trace = new Trace(), seen = observation(), cache = new Map();
    let operation = 'CLICK';
    const choose = () => ({ operation, target: operation === 'CLICK' ? '2' : null });
    const policy = makePolicy(arm, { upstream, trace, qwen: fakeQwen(trace, choose), laya: fakeLaya(choose) });
    await policy.decide(seen, 'Submit', [], cache);
    await policy.decide(seen, 'Submit', [], cache);
    assert.equal(trace.counts().decision_cache_hits, 1);
    operation = 'WAIT';
    await policy.decide(seen, 'Wait for update', [], cache);
    await policy.decide(seen, 'Wait for update', [], cache);
    assert.equal(trace.counts().decision_cache_hits, 1);
    assert.equal(arm === 'A' ? trace.counts().qwen_http_attempts : trace.counts().laya_predict_calls, 3);
  });
}
for (const answer of [{ operation: 'DELETE', target: '1' }, { operation: 'CLICK', target: '999' },
  { operation: 'TYPE_TEXT', target: '1', text: 'submit\n' }, { operation: 'CLICK', target: '2', text: 'injected' }]) {
  test(`invalid Qwen selection is never executed: ${JSON.stringify(answer)}`, async () => {
    const trace = new Trace();
    const policy = makePolicy('A', { upstream, trace, qwen: fakeQwen(trace, () => answer, { max_attempts: 1 }) });
    await assert.rejects(policy.decide(observation(), 'Test', [], new Map()));
    assert.equal(trace.counts().execution_attempts, 0);
  });
}
test('Laya validation retries an invalid selected head, not irrelevant heads', async () => {
  let calls = 0;
  const trace = new Trace(), seen = observation();
  const laya = { predict: async body => {
    calls++;
    return { answers: { operation: choice(body.questions.operation.criteria, 'CLICK'),
      click_target: calls === 1 ? null : choice(body.questions.click_target.criteria, '2'), type_text_target: null } };
  } };
  const policy = makePolicy('B', { upstream, trace, laya });
  assert.equal((await policy.decide(seen, 'Click', [], new Map())).choice, 'click-submit');
  assert.equal(calls, 2);
});
test('retry accounting includes failed requests and keeps unknown tokens null', async () => {
  let n = 0;
  const trace = new Trace();
  const qwen = new QwenClient(config, trace, { apiKey: 'test-only', fetchImpl: async () =>
    ++n === 1 ? new Response('Do not log this body', { status: 500 }) : response({ text: '' }) });
  assert.equal(await qwen.json('generation', '', {}, { accept: textValue }), '');
  assert.equal(trace.counts().qwen_http_attempts, 2);
  assert.equal(trace.counts().qwen_logical_requests, 1);
  assert.equal(trace.counts().qwen_retries, 1);
  assert.equal(trace.counts().qwen_tokens.total_tokens, null);
});
test('HTTP timing includes the response body, not just headers', async () => {
  const trace = new Trace();
  const qwen = new QwenClient(config, trace, { apiKey: 'test-only', fetchImpl: async () => ({
    status: 200, ok: true, json: async () => { await sleep(40); return response({ text: 'ok' }).json(); },
  }) });
  await qwen.json('generation', '', {}, { accept: textValue });
  assert.ok(trace.events[0].elapsed_ms >= 35);
});
test('aborted input never dispatches HTTP; unapproved endpoint is rejected', async () => {
  const trace = new Trace(), qwen = fakeQwen(trace, () => assert.fail('No call permitted'));
  await assert.rejects(qwen.json('decision', '', {}, { signal: AbortSignal.abort() }));
  assert.equal(trace.counts().qwen_http_attempts, 0);
  assert.throws(() => new QwenClient({ ...config, endpoint: 'https://example.com' }, trace, { apiKey: 'test' }));
});
test('traces redact registered credentials and record errors without their messages', async () => {
  const directory = mkdtempSync(join(tmpdir(), 'jev-trace-test-'));
  const file = join(directory, 'trace.jsonl'), trace = new Trace(file);
  try {
    trace.protect('private-placeholder');
    await assert.rejects(trace.measure('test', 'error', async event => {
      event.value = 'private-placeholder'; throw new Error('private-placeholder');
    }));
    trace.close();
    const content = readFileSync(file, 'utf8');
    assert.ok(!content.includes('private-placeholder'));
    assert.ok(content.includes('[REDACTED]'));
  } finally { trace.close(); rmSync(file); rmdirSync(directory); }
});
test('unknown interrupted Laya forward counts remain unknown', async () => {
  const trace = new Trace();
  await assert.rejects(trace.measure('laya', 'decision', async () => { throw new Error('stopped'); },
    { forward_starts: 1, actual_calls: null, forward_count_complete: false }));
  assert.equal(trace.counts().laya_forward_attempts, null);
  assert.equal(trace.counts().laya_observed_forward_starts, 1);
});

function fakePage(url, story = '123') {
  return { url: () => url, locator: () => ({ first: () => ({ getAttribute: async () => story }) }) };
}
test('HN evaluator rejects another story even when the upstream URL check passes', async () => {
  const task = upstream.journeys.find(j => j.name === 'hacker-news');
  const evaluator = await prepareAcceptance(task, fakePage('https://news.ycombinator.com'));
  const wrong = await evaluator.check(fakePage('https://news.ycombinator.com/item?id=456'));
  assert.equal(wrong.upstream_pass, true); assert.equal(wrong.strict_pass, false);
  assert.equal((await evaluator.check(fakePage('https://news.ycombinator.com/item?id=123'))).strict_pass, true);
});
test('MDN evaluator rejects another map page and wrong host', async () => {
  const task = upstream.journeys.find(j => j.name === 'mdn');
  const evaluator = await prepareAcceptance(task, fakePage('https://developer.mozilla.org'));
  assert.equal((await evaluator.check(fakePage('https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/TypedArray/map'))).strict_pass, false);
  const path = '/en-US/docs/Web/JavaScript/Reference/Global_Objects/Array/map';
  assert.equal((await evaluator.check(fakePage('https://developer.mozilla.org' + path))).strict_pass, true);
  assert.equal((await evaluator.check(fakePage('https://example.com' + path))).strict_pass, false);
});
test('summary keeps failures in denominators, excludes unmatched success from speedup', () => {
  const row = (task_id, arm, success, elapsed_ms) => ({ task_id, arm, success, elapsed_ms,
    repetition: 0, total_ms: elapsed_ms + 1 });
  const summary = summarize([row('one', 'A', true, 100), row('one', 'B', true, 50),
    row('two', 'A', true, 100), row('two', 'B', false, 1), row('three', 'A', false, 5)]);
  assert.equal(summary.arms.B.pass_rate, 0.5);
  assert.equal(summary.arms.B.all.n, 2);
  assert.equal(summary.paired_speedup, 2);
  assert.equal(summary.only_A, 1); assert.equal(summary.incomplete_pairs, 1);
  assert.equal(summarize([row('x', 'A', false, 1)]).paired_speedup, null);
});

// Scripted responses exist only in engineering tests, never in either research policy.
function scripted(task, body) {
  const { state, questions } = body;
  if (task === 'shadow-dom' || task === 'cross-origin-iframe') {
    const text = task === 'shadow-dom' ? 'ada@example.com' : 'SAVE10';
    if (state.page.text.includes('submitted:' + text) || state.page.text.includes('applied:' + text))
      return { operation: 'DONE', target: null };
    const fill = Object.entries(questions.type_text_target?.criteria ?? {}).find(([, c]) => c.current_value !== text);
    if (fill) return { operation: 'TYPE_TEXT', target: fill[0], text };
    const click = Object.entries(questions.click_target.criteria).find(([, c]) => /Submit|Apply/.test(c.element));
    assert.ok(click); return { operation: 'CLICK', target: click[0] };
  }
  if (task === 'select') {
    if (state.page.text.includes('tier:vip')) return { operation: 'DONE', target: null };
    const option = Object.entries(questions.select_target.criteria).find(([, c]) => /VIP/.test(c.element));
    assert.ok(option); return { operation: 'SELECT', target: option[0] };
  }
  if (state.page.title === 'row-47') return { operation: 'DONE', target: null };
  const target = Object.entries(questions.click_target?.criteria ?? {}).find(([, c]) => /"Row 47"/.test(c.element));
  if (target) return { operation: 'CLICK', target: target[0] };
  const down = Object.entries(questions.scroll_down_target?.criteria ?? {});
  return { operation: 'SCROLL_DOWN', target: down.length ? down[0][0] : null };
}
const limits = { timeout_ms: 30000, max_actions: 30, max_decisions: 60, browser_timeout_ms: 3000 };
for (const name of ['shadow-dom', 'nested-scroll', 'cross-origin-iframe', 'select']) {
  for (const arm of ['A', 'B']) {
    test(`real Chromium, upstream ${name}, ${arm} with scripted model responses`, async () => {
      const trace = new Trace(), task = upstream.journeys.find(j => j.name === name);
      const qwen = fakeQwen(trace, input => input.questions ? scripted(name, input)
        : { text: name === 'shadow-dom' ? 'ada@example.com' : 'SAVE10' });
      const policy = makePolicy(arm, { upstream, trace, qwen, laya: fakeLaya(body => scripted(name, body)) });
      const result = await runAttempt({ upstream, browser, task, url: task.url(local.port, foreign.port),
        arm, repetition: 0, limits, policy, trace });
      assert.equal(result.success, true, JSON.stringify(result));
      assert.equal(result.acceptance.upstream_pass, true);
      assert.ok(result.elapsed_ms > 0 && result.total_ms >= result.elapsed_ms);
      assert.ok(result.counts.execution_attempts > 0);
      if (arm === 'B') assert.equal(result.counts.qwen_http_attempts,
        ['shadow-dom', 'cross-origin-iframe'].includes(name) ? 1 : 0);
    });
  }
}
test('model DONE cannot pass an unfinished browser task', async () => {
  const trace = new Trace(), task = upstream.journeys.find(j => j.name === 'select');
  const policy = makePolicy('A', { upstream, trace, qwen: fakeQwen(trace, () => ({ operation: 'DONE', target: null })) });
  const result = await runAttempt({ upstream, browser, task, url: task.url(local.port), arm: 'A', repetition: 0,
    limits, policy, trace });
  assert.equal(result.success, false);
  assert.equal(result.upstream.status, 'done');
  assert.equal(result.acceptance.strict_pass, false);
});
test('deadline includes initial navigation and records a timed failure', async () => {
  const trace = new Trace(), task = upstream.journeys[0];
  const result = await runAttempt({ upstream, browser, task, url: task.url(local.port), arm: 'A', repetition: 0,
    limits: { ...limits, timeout_ms: 1 }, policy: {}, trace });
  assert.equal(result.success, false); assert.equal(result.termination, 'timeout');
  assert.ok(result.elapsed_ms > 0); assert.equal(result.counts.qwen_http_attempts, 0);
});
