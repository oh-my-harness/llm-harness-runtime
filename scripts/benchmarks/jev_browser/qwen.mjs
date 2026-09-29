import { setTimeout as sleep } from 'node:timers/promises';

export const QWEN_MODEL = 'Qwen/Qwen2.5-72B-Instruct';
const ENDPOINT = 'https://api.siliconflow.cn/v1/chat/completions';

export class ProtocolError extends Error {
  constructor() { super('Invalid structured model response'); this.name = 'ProtocolError'; }
}

export function textValue(object) {
  if (!object || Array.isArray(object) || Object.keys(object).join(',') !== 'text'
    || typeof object.text !== 'string' || object.text.length >= 2000
    || /[\u0000-\u001f\u007f]/u.test(object.text)) throw new ProtocolError();
  return object.text;
}

export class QwenClient {
  constructor(config, trace, { apiKey = process.env.SILICONFLOW_API_KEY, fetchImpl = fetch } = {}) {
    if (config.model !== QWEN_MODEL || config.endpoint !== ENDPOINT) throw new Error('Unapproved model or credential destination');
    for (const key of ['max_tokens', 'timeout_ms', 'max_attempts'])
      if (!Number.isInteger(config[key]) || config[key] <= 0) throw new Error(`Missing Qwen ${key}`);
    if (!Number.isFinite(config.temperature) || !apiKey) throw new Error('Missing Qwen parameters or credential');
    this.config = config; this.trace = trace; this.apiKey = apiKey; this.fetch = fetchImpl;
    this.logical = 0;
    trace.protect(apiKey);
  }

  async json(purpose, system, input, { signal, accept = value => value } = {}) {
    const logicalId = ++this.logical;
    for (let retry = 0; retry < this.config.max_attempts; retry++) {
      signal?.throwIfAborted();
      let retryable = false;
      try {
        return await this.trace.measure('qwen', purpose, async event => {
          const body = { model: this.config.model, temperature: this.config.temperature,
            max_tokens: this.config.max_tokens, stream: false, response_format: { type: 'json_object' },
            messages: [{ role: 'system', content: system }, { role: 'user', content: JSON.stringify(input) }] };
          event.request = body;
          const bounded = AbortSignal.timeout(this.config.timeout_ms);
          const requestSignal = signal ? AbortSignal.any([signal, bounded]) : bounded;
          requestSignal.throwIfAborted();
          event.actual_calls = 1;
          this.trace.emit({ phase: 'http_start', id: event.id, logical_id: logicalId, retry_index: retry });
          const response = await this.fetch(ENDPOINT, { method: 'POST', redirect: 'error', signal: requestSignal,
            headers: { Authorization: `Bearer ${this.apiKey}`, 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
          event.http_status = response.status;
          if (!response.ok) {
            retryable = [429, 500, 502, 503, 504, 520, 521, 522, 523, 524, 529].includes(response.status);
            await response.body?.cancel();
            throw new Error('Provider HTTP failure');
          }
          let payload;
          try { payload = await response.json(); }
          catch (error) { if (requestSignal.aborted) throw error; throw new ProtocolError(); }
          if (!payload || typeof payload !== 'object') throw new ProtocolError();
          event.usage = payload.usage ?? null;
          event.returned_model = payload.model ?? null;
          event.finish_reason = payload.choices?.[0]?.finish_reason ?? null;
          requestSignal.throwIfAborted();
          let object;
          try { object = JSON.parse(payload.choices?.[0]?.message?.content); }
          catch { throw new ProtocolError(); }
          const answer = accept(object);
          event.answer = object;
          return answer;
        }, { logical_id: logicalId, retry_index: retry, actual_calls: 0, usage: null });
      } catch (error) {
        if (signal?.aborted || retry + 1 === this.config.max_attempts
          || !(retryable || error instanceof ProtocolError)) throw error;
        await this.trace.measure('retry_wait', 'qwen', () => sleep(250 * (retry + 1), undefined, { signal }));
      }
    }
  }
}
