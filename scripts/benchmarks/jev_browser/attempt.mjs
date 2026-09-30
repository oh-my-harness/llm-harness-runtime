import { prepareAcceptance } from './acceptance.mjs';

// Model initialization belongs to the caller's separately measured session setup.
export async function runAttempt({ upstream, browser, task, url, arm, repetition,
  limits, policy, trace, signal }) {
  if (!['A', 'B'].includes(arm)) throw new Error('Invalid arm');
  for (const key of ['timeout_ms', 'max_actions', 'max_decisions', 'browser_timeout_ms'])
    if (!Number.isInteger(limits[key]) || limits[key] <= 0) throw new Error(`Missing attempt ${key}`);
  const totalStart = performance.now();
  const result = { task_id: task.name, arm, repetition, success: false, elapsed_ms: null,
    termination: 'setup_error', initial: null, acceptance: null, upstream: null };
  let context, start, timer, work;
  const controller = new AbortController();
  const abort = () => { controller.abort(signal.reason); void context?.close().catch(() => {}); };
  signal?.addEventListener('abort', abort, { once: true });
  try {
    signal?.throwIfAborted();
    context = await trace.measure('browser_setup', 'context', () => browser.newContext({
      viewport: { width: 1120, height: 780 }, acceptDownloads: false, serviceWorkers: 'block',
    }));
    context.setDefaultTimeout(Math.min(limits.browser_timeout_ms, limits.timeout_ms));
    const page = await trace.measure('browser_setup', 'page', () => context.newPage());
    let documentStatus = null;
    page.on('response', response => {
      if (response.request().isNavigationRequest() && response.frame() === page.mainFrame())
        documentStatus = response.status();
    });
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
        const response = await page.goto(url, { waitUntil: 'load' });
        result.initial_http_status = response?.status() ?? null;
        if (response && !response.ok()) throw new Error('Initial page HTTP failure');
        await page.waitForLoadState('networkidle');
      });
      const evaluator = await trace.measure('acceptance', 'prepare', () => prepareAcceptance(task, page));
      result.initial = evaluator.initial;
      const initial = await trace.measure('initial_snapshot', 'provenance', () =>
        upstream.observe(context, { screenshot: false, diagnostics: false }));
      result.initial.fingerprint = initial.fingerprint;
      result.initial.title = initial.title;
      const run = await upstream.run(context, { goal: task.goal, policy, signal: controller.signal,
        maxActions: limits.max_actions, maxDecisions: limits.max_decisions,
        browserTimeoutMs: Math.min(limits.browser_timeout_ms, limits.timeout_ms),
        measure: (phase, execute) => trace.measure('engine', phase, execute) });
      result.upstream = { status: run.status, reason: run.reason, elapsed_ms: run.elapsedMs,
        decisions: run.decisions.length, actions: run.history.length, usage: run.usage };
      // Preserve upstream's original-page scoring (no policy-visible end-condition feedback).
      result.acceptance = await trace.measure('acceptance', 'final', () => evaluator.check(page));
      result.acceptance.document_http_status = documentStatus;
      result.acceptance.strict_pass = result.acceptance.strict_pass
        && documentStatus !== null && documentStatus >= 200 && documentStatus < 400;
      controller.signal.throwIfAborted();
      result.success = result.acceptance.strict_pass;
      result.termination = result.success ? 'passed' : 'not_passed';
    })();
    await Promise.race([work, deadline]);
  } catch (error) {
    result.termination = signal?.aborted ? 'session_timeout' : start === undefined ? 'setup_error'
      : controller.signal.aborted ? 'timeout' : error.name === 'BudgetError' ? 'budget_exhausted'
        : error.name === 'InputBudgetError' ? 'input_rejected' : 'error';
    result.error_type = error.name;
  } finally {
    clearTimeout(timer);
    signal?.removeEventListener('abort', abort);
    if (start !== undefined) result.elapsed_ms = performance.now() - start;
    if (context) await trace.measure('cleanup', 'context', () => context.close()).catch(() => {});
    // Browser closure and model cancellation settle in-flight trace events before the snapshot.
    await work?.catch(() => {});
    result.total_ms = performance.now() - totalStart;
    result.counts = trace.counts();
  }
  return result;
}
