# Experiment: Fast Small Model Agent Loop vs Standard Large Model ReAct Loop

## Goal

Measure the speed and task-completion trade-off of using a **small/fast LLM** (target: ~0.1 s per call) for per-step agent-loop decisions, compared to the standard **large LLM** (~2 s per call) in the same sequential ReAct loop.

## Hypothesis

The agent loop remains **sequential ReAct** (observe -> decide -> act -> observe). The only change is the **model used for per-step decisions**: a small model (e.g. 0.5B-3B params) that responds in ~0.1 s instead of a large model (~2 s). Because tool calls in software domains are already fast (< 100 ms), reducing LLM latency from ~2 s to ~0.1 s per step should make the entire loop 2-10x faster for tasks with 5-15 steps.

Task-completion quality may decrease because the small model has weaker reasoning. We aim to quantify this gap.

## Two Modes Under Test

| Aspect | Large Model ReAct (baseline) | Small Model ReAct (experimental) |
|--------|------------------------------|----------------------------------|
| LLM calls | One per step | One per step (same) |
| Control flow | Sequential ReAct | Sequential ReAct (same) |
| Model per step | Large (e.g. Qwen-72B, GPT-4o) ~2 s/call | Small (e.g. Qwen-0.5B, Phi-mini) ~0.1 s/call |
| Structured output | Yes (tool call or JSON choice) | Yes (structured output: choice/action) |
| Adaptive to errors | Yes | Yes (same, just faster decisions) |
| Expected speed | Baseline | ~2-10x faster (fewer LLM wait per step) |
| Expected quality | Baseline | May degrade on complex reasoning tasks |

## Experiment Design

### Task Suite

Prepare 10-20 tasks in a sandboxed repository that the agent must complete end-to-end. Tasks should vary in:

- **Reasoning complexity** (simple tool selection -> requires multi-hop reasoning)
- **Step count** (3 -> 15 steps)
- **Tool type** (file ops, code search, shell commands)

Example tasks:

1. Read a file, find a function, rename it across the repo.
2. Search for a bug pattern, fix it, run tests.
3. Create a new module, wire it into the entry point, update docs.
4. Multi-step debugging: read logs -> locate error -> patch -> verify.

Each task has a **known-correct expected outcome** verified independently.

### Model Candidates

Check [SiliconFlow](https://siliconflow.cn) for available small/fast models. Candidates:

| Role | Model | Expected latency/call |
|------|-------|----------------------|
| Baseline (large) | Qwen-72B / DeepSeek-V3 / GPT-4o | ~1-3 s |
| Fast (small) | Qwen2.5-0.5B / Qwen2.5-1.5B / Phi-3-mini / Llama-3.2-1B | ~0.05-0.2 s |

> Note: identify the specific "javi/jev" model with the team before running.

### Metrics

| Metric | How to measure |
|--------|---------------|
| Total wall-clock time | `Instant::now()` around the full loop |
| Per-step LLM latency | Timestamp each provider request |
| Per-step tool latency | Timestamp each tool dispatch |
| LLM total time vs tool total time | Sum separately |
| Task success | Binary: final state matches expected outcome? |
| Partial completion | % of expected sub-goals achieved |

### Runner Architecture

Create a benchmark binary or integration test in `crates/llm-harness-loop` that:

1. Sets up a temp workspace with known files.
2. Runs the same task with **mode A (large model)** and **mode B (small model)**.
3. Records per-step timing, call counts, and success/failure.
4. Outputs a comparison table.

### Controls

- Same loop logic, same tools, same workspace.
- Only the model changes between runs.
- Same structured-output schema for tool selection.
- Run each task >= 3 times for variance.
- Record model name, endpoint, date.

## Success Criteria

| Criterion | Target |
|-----------|--------|
| Speed improvement (all tasks) | >= 2x wall-clock reduction |
| Task completion (simple tasks) | >= 90% of large-model baseline |
| Task completion (complex tasks) | Quantify gap; identify which steps degrade |
| Stability | Reproducible across 3 runs |

## Next Steps

1. Confirm the exact fast model ("javi/jev") available on SiliconFlow.
2. Implement the benchmark runner (mode switch: model config only).
3. Prepare the task suite.
4. Run and collect results.
5. Write up and decide whether to adopt small-model routing in production.
