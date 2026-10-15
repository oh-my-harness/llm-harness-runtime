import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, readdirSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const sha256 = value => createHash('sha256').update(value).digest('hex');
const sameKeys = (a, b) => assert.deepEqual(Object.keys(a).sort(), Object.keys(b).sort());

// Audits recorded model answers, not browser success or unrecorded model internals.
export function auditTrace(rows) {
  const decisions = [];
  let active = null, heads = 0;
  for (const row of rows) {
    if (row.component === 'policy' && row.phase === 'start') {
      assert.equal(row.purpose, 'B');
      assert.equal(active, null, 'Overlapping policy calls');
      active = { id: row.id, calls: [] };
    }
    if (row.component === 'laya' && row.phase === 'end') {
      assert.ok(active, 'Laya answer outside a policy call');
      assert.equal(row.status, 'ok');
      assert.equal(row.input_transport_verified, true);
      assert.equal(sha256(JSON.stringify(row.request)), row.wire_sha256);
      assert.equal(row.wire_sha256, row.original_request_sha256);
      assert.equal(row.actual_calls, 1);
      assert.equal(row.forward_attempts, 1);
      assert.equal(row.cpu_fallback_count, 0);
      sameKeys(row.request.questions, row.answers);
      sameKeys(row.request.questions, row.input_audit);
      for (const [key, question] of Object.entries(row.request.questions)) {
        const answer = row.answers[key], audit = row.input_audit[key];
        assert.equal(question.type, 'choice');
        assert.equal(answer.type, 'choice');
        assert.ok(Object.hasOwn(question.criteria, answer.choice));
        sameKeys(question.criteria, answer.probabilities);
        const probabilities = Object.values(answer.probabilities);
        assert.ok(probabilities.every(p => Number.isFinite(p) && p >= 0 && p <= 1));
        assert.ok(Math.abs(probabilities.reduce((a, b) => a + b, 0) - 1) <= 0.02);
        assert.equal(answer.probabilities[answer.choice], Math.max(...probabilities));
        for (const field of ['state_tokens_dropped', 'head_tokens_dropped', 'option_tokens_dropped_at_48'])
          assert.equal(audit[field], 0);
        assert.equal(audit.options, Object.keys(question.criteria).length);
        assert.equal(audit.markers, audit.options);
        assert.equal(audit.options_distinct, audit.options);
        heads++;
      }
      active.calls.push(row);
    }
    if (row.component === 'policy' && row.phase === 'end') {
      assert.ok(active);
      assert.equal(row.id, active.id);
      assert.equal(row.status, 'ok');
      assert.equal(row.cache_hit, false, 'This audit requires uncached decisions');
      assert.equal(active.calls.length, 1);
      const call = active.calls[0], operation = call.answers.operation.choice;
      const head = call.answers[`${operation.toLowerCase()}_target`];
      const target = ['DONE', 'BLOCKED'].includes(operation) ? null : (head?.choice ?? null);
      assert.equal(row.selection.operation, operation);
      assert.equal(row.selection.target, target);
      if (['DONE', 'BLOCKED'].includes(operation)) assert.equal(row.selection.choice, operation);
      const criterion = target === null ? null : call.request.questions[`${operation.toLowerCase()}_target`].criteria[target];
      decisions.push({ operation, target, criterion,
        operation_probability: call.answers.operation.probabilities[operation],
        target_probabilities: head?.probabilities ?? null });
      active = null;
    }
  }
  assert.equal(active, null, 'Unfinished policy call');
  assert.ok(decisions.length, 'No decisions found');
  return { decisions_checked: decisions.length, heads_checked: heads, decisions };
}

export function auditDirectory(directory) {
  const names = readdirSync(directory).filter(name => /^\d+-.+-B\.jsonl$/.test(name)).sort();
  assert.ok(names.length, 'No B attempt traces found');
  const files = names.map(file => {
    const raw = readFileSync(resolve(directory, file));
    const rows = raw.toString('utf8').trim().split(/\r?\n/).map(JSON.parse);
    return { file, sha256: sha256(raw), ...auditTrace(rows) };
  });
  return { scope: 'recorded_uncached_B_choices_only', model_calls: 0,
    attempts_checked: files.length,
    decisions_checked: files.reduce((n, f) => n + f.decisions_checked, 0),
    heads_checked: files.reduce((n, f) => n + f.heads_checked, 0), files };
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  assert.equal(process.argv.length, 3, 'Usage: node audit-traces.mjs RUN_DIRECTORY');
  console.log(JSON.stringify(auditDirectory(resolve(process.argv[2])), null, 2));
}
