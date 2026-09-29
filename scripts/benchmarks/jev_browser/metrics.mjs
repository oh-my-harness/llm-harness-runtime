import { openSync, writeSync, closeSync } from 'node:fs';

export class Trace {
  constructor(path = null) {
    this.events = [];
    this.sequence = 0;
    this.secrets = [];
    this.fd = path === null ? null : openSync(path, 'wx');
  }

  protect(secret) { if (secret) this.secrets.push(secret); }

  emit(record) {
    let line = JSON.stringify(record);
    for (const secret of this.secrets) line = line.split(secret).join('[REDACTED]');
    if (this.fd !== null) writeSync(this.fd, line + '\n');
  }

  async measure(component, purpose, work, fields = {}) {
    const event = { id: ++this.sequence, component, purpose, ...fields, status: 'running' };
    const start = performance.now();
    this.emit({ phase: 'start', ...event });
    try {
      const result = await work(event);
      event.status = 'ok';
      return result;
    } catch (error) {
      event.status = 'error';
      event.error_type = error?.name ?? 'Error';
      throw error;
    } finally {
      event.elapsed_ms = performance.now() - start;
      this.events.push(event);
      this.emit({ phase: 'end', ...event });
    }
  }

  counts() {
    const qwen = this.events.filter(e => e.component === 'qwen');
    const laya = this.events.filter(e => e.component === 'laya');
    const known = (events, key) => events.every(e => Number.isFinite(e[key]))
      ? events.reduce((n, e) => n + e[key], 0) : null;
    return {
      qwen_http_attempts: qwen.reduce((n, e) => n + (e.actual_calls ?? 0), 0),
      qwen_logical_requests: new Set(qwen.map(e => e.logical_id)).size,
      qwen_by_purpose: Object.fromEntries(['decision', 'mixed', 'generation'].map(p =>
        [p, qwen.filter(e => e.purpose === p).reduce((n, e) => n + (e.actual_calls ?? 0), 0)])),
      qwen_retries: qwen.filter(e => e.retry_index > 0 && e.actual_calls === 1).length,
      qwen_tokens: Object.fromEntries(['prompt_tokens', 'completion_tokens', 'total_tokens'].map(k =>
        [k, qwen.length === 0 ? 0 : qwen.every(e => Number.isFinite(e.usage?.[k]))
          ? qwen.reduce((n, e) => n + e.usage[k], 0) : null])),
      laya_rpc_attempts: laya.length,
      laya_predict_calls: known(laya, 'actual_calls'),
      laya_decision_questions: known(laya, 'decision_questions'),
      laya_forward_attempts: laya.every(e => e.forward_count_complete)
        ? known(laya, 'forward_attempts') : null,
      laya_observed_forward_starts: laya.reduce((n, e) => n + (e.forward_starts ?? 0), 0),
      execution_attempts: this.events.filter(e => e.component === 'engine' && e.purpose === 'execute').length,
      decision_cache_hits: this.events.filter(e => e.component === 'policy' && e.cache_hit).length,
      component_ms: Object.fromEntries([...new Set(this.events.map(e => e.component))].map(c =>
        [c, this.events.filter(e => e.component === c).reduce((n, e) => n + e.elapsed_ms, 0)])),
    };
  }

  close() { if (this.fd !== null) closeSync(this.fd); this.fd = null; }
}

function distribution(values) {
  if (!values.length) return { n: 0, mean_ms: null, median_ms: null, p95_ms: null };
  const sorted = [...values].sort((a, b) => a - b);
  const quantile = q => {
    const x = (sorted.length - 1) * q, i = Math.floor(x);
    return sorted[i] + (sorted[Math.ceil(x)] - sorted[i]) * (x - i);
  };
  return { n: values.length, mean_ms: values.reduce((a, b) => a + b, 0) / values.length,
    median_ms: quantile(0.5), p95_ms: quantile(0.95) };
}

export function summarize(results) {
  const arms = Object.fromEntries(['A', 'B'].map(arm => {
    const rows = results.filter(r => r.arm === arm), passed = rows.filter(r => r.success);
    const timed = rows.filter(r => Number.isFinite(r.elapsed_ms));
    return [arm, { attempts: rows.length, passed: passed.length,
      pass_rate: rows.length ? passed.length / rows.length : null,
      all: distribution(timed.map(r => r.elapsed_ms)),
      success: distribution(passed.filter(r => Number.isFinite(r.elapsed_ms)).map(r => r.elapsed_ms)),
      failure: distribution(timed.filter(r => !r.success).map(r => r.elapsed_ms)),
      setup_failures: rows.filter(r => r.termination === 'setup_error').length,
      amortized_ms_per_success: passed.length && rows.every(r => Number.isFinite(r.total_ms))
        ? rows.reduce((n, r) => n + r.total_ms, 0) / passed.length : null }];
  }));
  const pairs = new Map();
  for (const row of results) {
    const key = `${row.task_id}:${row.repetition}`;
    if (!pairs.has(key)) pairs.set(key, {});
    if (pairs.get(key)[row.arm]) throw new Error('Duplicate paired attempt');
    pairs.get(key)[row.arm] = row;
  }
  const complete = [...pairs.values()].filter(p => p.A && p.B);
  const both = complete.filter(p => p.A.success && p.B.success);
  const comparable = both.filter(p => p.A.initial?.expected_story_id === p.B.initial?.expected_story_id);
  const denominator = comparable.reduce((n, p) => n + p.B.elapsed_ms, 0);
  return { arms, quality_delta_pp: arms.A.pass_rate === null || arms.B.pass_rate === null
    ? null : 100 * (arms.B.pass_rate - arms.A.pass_rate),
    complete_pairs: complete.length, incomplete_pairs: pairs.size - complete.length,
    both_pass: both.length, only_A: complete.filter(p => p.A.success && !p.B.success).length,
    only_B: complete.filter(p => !p.A.success && p.B.success).length,
    both_fail: complete.filter(p => !p.A.success && !p.B.success).length,
    speed_pairs: comparable.length, top_story_drift_pairs: both.length - comparable.length,
    paired_speedup: denominator > 0 ? comparable.reduce((n, p) => n + p.A.elapsed_ms, 0) / denominator : null };
}
