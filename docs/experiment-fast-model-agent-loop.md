# Experiment: Large vs Small Model Agent Loop

## Purpose

Determine how much faster a local small model completes agent tasks than a cloud
large model, and whether task accuracy is maintained. Both arms use the same
ReAct loop, tasks, prompts, tools, budgets and independent final-state validators.

- Large: SiliconFlow Qwen2.5-72B-Instruct.
- Small: local Ollama Qwen3.5-4B on RTX 5070 Ti.

Record task elapsed time, individual and cumulative model/tool timings,
model/tool call counts, task accuracy and false completion. Retain failures in
the denominator and distinguish success-only timing from all-attempt timing.
This compares deployments; network, quantization and inference backends also
affect results.

The canonical protocol, measured results, limitations and reproduction commands
are in [the experiment report](research/2026-09-28-local-decision-benchmark.md).
The runner is `scripts/benchmark_jev_real.mjs`. Simulated benchmark results do
not establish real-model speed or accuracy and are not part of this comparison.
