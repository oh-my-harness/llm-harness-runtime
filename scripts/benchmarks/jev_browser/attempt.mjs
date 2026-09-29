import { prepareAcceptance } from './acceptance.mjs';

// Model initialization belongs to the caller's separately measured session setup.
export async function runAttempt({ upstream, browser, task, url, arm, repetition,
  limits, policy, trace }) {
  if (!['A', 'B'].includes(arm)) throw new Error('Invalid arm');
  for (const key of ['timeout_ms', 'max_actions', 'max_decisions', 'browser_timeout_ms'])
    if (!Number.isInteger(limits[key]) || limits[key] <= 0) throw new Error(`Missing attempt ${key}`);
  const totalStart = performance.now();
  const result = { task_id: task.name, arm, repetition, success: false, elapsed_ms: null,
    termination: 'setup_error', initial: null, acceptance: null, upstream: null };
  let context, start, timer, work;
  const controller = new AbortController();
  try {
    context = await trace.measure('browser_setup', 'context', () => browser.newContext({
      viewport: { width: 1120, height: 780 }, acceptDownloads: false, serviceWorkers: 'block',
    }));
    context.setDefaultTimeout(Math.min(limits.browser_timeout_ms, limits.timeout_ms));
    const page = await trace.measure('browser_setup', 'page', () => context.newPage());
    start = performance.now();
    const deadline = new Promise((_, reject) => {
      timer = setTimeout(() => {
        controller.abort(new Error('Task deadline exceeded'));
        void context.close().catch(() => {});
        reject(controller.signal.reason);
      }, limits.timeout_ms);
    });
    work = (async () => {
      await trace.measure('navigation', 'initial_page', async () => {
        await page.goto(url, { waitUntil: 'load' });
        await page.waitForLoadState('networkidle');
      });
      const evaluator = await trace.measure('acceptance', 'prepare', () => prepareAcceptance(task, page));
      result.initial = evaluator.initial;
      const run = await upstream.run(context, { goal: task.goal, policy, signal: controller.signal,
        maxActions: limits.max_actions, maxDecisions: limits.max_decisions,
        browserTimeoutMs: Math.min(limits.browser_timeout_ms, limits.timeout_ms),
        measure: (phase, execute) => trace.measure('engine', phase, execute) });
      result.upstream = { status: run.status, reason: run.reason, elapsed_ms: run.elapsedMs,
        decisions: run.decisions.length, actions: run.history.length, usage: run.usage };
      // Preserve upstream's original-page scoring (no policy-visible end-condition feedback).
      result.acceptance = await trace.measure('acceptance', 'final', () => evaluator.check(page));
      controller.signal.throwIfAborted();
      result.success = result.acceptance.strict_pass;
      result.termination = result.success ? 'passed' : 'not_passed';
    })();
    await Promise.race([work, deadline]);
  } catch (error) {
    result.termination = start === undefined ? 'setup_error' : controller.signal.aborted ? 'timeout' : 'error';
    result.error_type = error.name;
  } finally {
    clearTimeout(timer);
    if (start !== undefined) result.elapsed_ms = performance.now() - start;
    if (context) await trace.measure('cleanup', 'context', () => context.close()).catch(() => {});
    // Browser closure and model cancellation settle in-flight trace events before the snapshot.
    await work?.catch(() => {});
    result.total_ms = performance.now() - totalStart;
    result.counts = trace.counts();
  }
  return result;
}
