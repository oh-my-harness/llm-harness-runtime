import { loadSource } from './source.mjs';

if (process.argv.length !== 2) throw new Error('This preparation command takes no live-run arguments');
const source = await loadSource();
if (source.config.live_run.enabled !== false) throw new Error('Live configuration needs a separate approved runner');
console.log(JSON.stringify({ status: 'offline_preparation_only', upstream: source.revision,
  tasks: source.journeys.map(({ name, goal }) => ({ name, goal })),
  model_calls: 0, note: 'Build and source checks only; no browser task or real model was run.' }, null, 2));
