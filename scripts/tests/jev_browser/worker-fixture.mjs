import { createInterface } from 'node:readline';

const lines = createInterface({ input: process.stdin });
const emit = value => process.stdout.write(JSON.stringify(value) + '\n');
for await (const line of lines) {
  const request = JSON.parse(line);
  const { id, method } = request;
  const body = request.body_json ? JSON.parse(request.body_json) : request.body;
  if (method === 'init') {
    emit({ id, ok: true, setup: { test_double: true, secret_present: Boolean(process.env.JEV_TEST_API_KEY) } });
    continue;
  }
  emit({ id, event: 'forward_started' });
  if (body.state === 'hang') continue;
  if (body.state === 'exit') { process.exit(1); }
  if (body.state === 'bad-id') { emit({ id: -1, ok: true }); continue; }
  emit({ id, ok: true, answers: {}, metrics: { actual_calls: 1, forward_attempts: 1,
    decision_questions: 1, usage: null } });
}
