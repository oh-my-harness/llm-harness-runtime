import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import { resolve } from 'node:path';
import { ROOT } from './source.mjs';

export class LayaWorker {
  constructor(config, { python = resolve(ROOT, '../benchmark/tools/laya-venv',
    process.platform === 'win32' ? 'Scripts/python.exe' : 'bin/python'), spawnImpl = spawn,
    script = fileURLToPath(new URL('./laya_worker.py', import.meta.url)) } = {}) {
    this.config = config; this.python = python; this.spawn = spawnImpl; this.script = script;
    this.nextId = 0; this.child = null; this.pending = null; this.ready = false;
  }

  async start(trace, signal) {
    if (this.ready) return;
    if (this.child) await this.close();
    const env = Object.fromEntries(Object.entries(process.env).filter(([name]) => !/(API_KEY|TOKEN|SECRET)$/i.test(name)));
    Object.assign(env, { HF_HUB_OFFLINE: '1', TRANSFORMERS_OFFLINE: '1', PYTHONUNBUFFERED: '1',
      HF_HOME: resolve(ROOT, '../benchmark/models/huggingface') });
    this.child = this.spawn(this.python, [this.script], { env, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'] });
    this.child.stderr.resume();
    const child = this.child;
    this.exited = new Promise(resolveExit => child.once('close', resolveExit));
    this.reader = createInterface({ input: child.stdout });
    this.reader.on('line', line => {
      try {
        const message = JSON.parse(line), pending = this.pending;
        if (!pending || message.id !== pending.id) throw new Error('Worker response ID mismatch');
        if (message.event === 'forward_started') { pending.progress(); return; }
        pending.finish(message);
      } catch { this.fail(new Error('Invalid worker protocol')); }
    });
    child.on('error', () => this.fail(new Error('Worker process error')));
    child.on('exit', () => { this.ready = false; this.fail(new Error('Worker exited')); });
    try {
      await trace.measure('model_setup', 'laya_load_and_warmup', async event => {
        const message = await this.request('init', { config: this.config, allow_model_load: true },
          signal, this.config.load_timeout_ms);
        if (!message.ok) {
          const error = new Error('Laya initialization failed');
          error.name = /^[A-Za-z0-9_]+$/.test(message.error_type) ? message.error_type : 'WorkerError';
          throw error;
        }
        event.setup = message.setup;
      });
      this.ready = true;
    } catch (error) { await this.close(); throw error; }
  }

  fail(error) {
    this.pending?.reject(error);
    this.ready = false;
    this.child?.kill();
  }

  request(method, payload, signal, timeoutMs, progress = () => {}) {
    signal?.throwIfAborted();
    if (this.pending || !this.child || this.child.exitCode !== null) throw new Error('Worker is unavailable or busy');
    if (!Number.isInteger(timeoutMs) || timeoutMs <= 0) throw new Error('Worker timeout is missing');
    return new Promise((resolveRequest, rejectRequest) => {
      const id = ++this.nextId;
      let timer;
      const clean = () => { clearTimeout(timer); signal?.removeEventListener('abort', abort); this.pending = null; };
      const abort = () => this.fail(signal?.reason ?? new Error('Worker timed out'));
      this.pending = { id, progress,
        finish: message => { clean(); resolveRequest(message); },
        reject: error => { clean(); rejectRequest(error); } };
      timer = setTimeout(abort, timeoutMs);
      signal?.addEventListener('abort', abort, { once: true });
      if (signal?.aborted) { abort(); return; }
      this.child.stdin.write(JSON.stringify({ id, method, ...payload }) + '\n', error => {
        if (error) this.fail(new Error('Worker stdin failure'));
      });
    });
  }

  async predict(body, trace, signal) {
    if (!this.ready) throw new Error('Laya must be initialized before task timing starts');
    return trace.measure('laya', 'decision', async event => {
      event.request = body;
      const message = await this.request('predict', { body }, signal, this.config.request_timeout_ms, () => {
        event.forward_starts++;
        trace.emit({ phase: 'forward_start', id: event.id, index: event.forward_starts });
      });
      Object.assign(event, message.metrics);
      event.forward_count_complete = Number.isInteger(message.metrics?.forward_attempts);
      if (!message.ok) {
        const error = new Error('Laya prediction failed');
        error.name = /^[A-Za-z0-9_]+$/.test(message.error_type) ? message.error_type : 'WorkerError';
        throw error;
      }
      return { answers: message.answers, usage: message.metrics?.usage };
    }, { actual_calls: null, forward_attempts: null, forward_starts: 0, forward_count_complete: false, usage: null });
  }

  async close() {
    if (!this.child) return;
    this.child.stdin.end();
    const timer = setTimeout(() => this.child?.kill(), 2000);
    await this.exited;
    clearTimeout(timer);
    this.reader.close(); this.child = null; this.ready = false;
  }
}
