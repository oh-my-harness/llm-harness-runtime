import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import test from 'node:test';
import { auditTrace } from '../../benchmarks/jev_browser/audit-traces.mjs';

function fixture() {
  const question = criteria => ({ type: 'choice', instructions: { goal: 'Select VIP' }, criteria });
  const request = { state: { text: 'Ticket' }, questions: {
    operation: question({ SELECT: 'Select an option', DONE: 'Goal satisfied' }),
    select_target: question({ '1:1': { element: 'Choose one' }, '1:3': { element: 'VIP' } }),
  } };
  const answer = (choice, probabilities) => ({ type: 'choice', choice, probabilities, confidence: 0.01 });
  const audit = () => ({ state_tokens_dropped: 0, head_tokens_dropped: 0,
    option_tokens_dropped_at_48: 0, options: 2, markers: 2, options_distinct: 2 });
  const hash = createHash('sha256').update(JSON.stringify(request)).digest('hex');
  return [
    { component: 'policy', phase: 'start', purpose: 'B', id: 1 },
    { component: 'laya', phase: 'end', id: 2, status: 'ok', request,
      input_transport_verified: true, wire_sha256: hash, original_request_sha256: hash,
      actual_calls: 1, forward_attempts: 1, cpu_fallback_count: 0,
      input_audit: { operation: audit(), select_target: audit() },
      answers: { operation: answer('SELECT', { SELECT: 0.8, DONE: 0.2 }),
        select_target: answer('1:1', { '1:1': 0.7, '1:3': 0.3 }) } },
    { component: 'policy', phase: 'end', id: 1, status: 'ok', cache_hit: false,
      selection: { operation: 'SELECT', target: '1:1', choice: 'opaque-action-id' } },
  ];
}

test('reports selected key and description without judging task success or confidence', () => {
  const result = auditTrace(fixture());
  assert.equal(result.heads_checked, 2);
  assert.deepEqual(result.decisions[0].criterion, { element: 'Choose one' });
});

for (const [name, change] of [
  ['non-argmax choice', rows => { rows[1].answers.select_target.choice = '1:3'; }],
  ['target remapping', rows => { rows[2].selection.target = '1:3'; }],
  ['operation override', rows => { rows[2].selection.operation = 'DONE'; }],
  ['missing probability', rows => { delete rows[1].answers.select_target.probabilities['1:3']; }],
  ['input truncation', rows => { rows[1].input_audit.operation.state_tokens_dropped = 1; }],
  ['changed request', rows => { rows[1].request.state.text = 'Different'; }],
  ['missing policy end', rows => { rows.pop(); }],
  ['cached policy', rows => { rows[2].cache_hit = true; }],
  ['duplicate model answer', rows => { rows.splice(2, 0, structuredClone(rows[1])); }],
]) test(`rejects ${name}`, () => {
  const rows = fixture();
  change(rows);
  assert.throws(() => auditTrace(rows));
});
