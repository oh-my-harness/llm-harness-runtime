# Experiment: Batch-Plan Agent Loop vs Standard ReAct Loop

## Goal

Measure the speed and task-completion trade-off of a **batch-plan + state-machine** agent loop against the current standard **ReAct (LLM-per-step)** loop in our runtime.

## Hypothesis

In domains where tool calls are fast (< 100 ms each, e.g. file read/write, code search, shell commands), the dominant latency in the ReAct loop is repeated LLM requests (~1-3 s each). By having the LLM output a multi-step structured plan up front and executing those steps via a deterministic state machine without intermediate LLM calls, the total loop wall-clock time should drop significantly (estimated 2-5x for linear tasks of 3-8 steps).

Task-completion quality may decrease for tasks requiring adaptive branching after each observation, because the batch plan lacks per-step observation feedback. We aim to quantify this gap on realistic software-engineering tasks.

## Two Modes Under Test

| Aspect | Standard ReAct (baseline) | Batch-Plan (experimental) |
|--------|---------------------------|---------------------------|
| LLM calls | Once per step | Once for the full plan; optional re-plan on error |
| Control flow | LLM decides each step after observing tool result | State machine executes plan sequentially |
| Tool execution | Sequential (after each LLM call) | Sequential or parallel (no LLM between steps) |
| Adaptive to errors | Yes (LLM sees every result) | No, until a re-plan is triggered |
| Expected speed | Baseline | ~2-5x faster for linear tool chains |
| Expected quality | Baseline | May degrade on branching/uncertain tasks |

## Experiment Design

### Task Suite

Prepare 10-20 tasks in a sandboxed repository (or temp directory) that the agent must complete end-to-end. Tasks should vary in:

- **Linearity** (fully linear -> requires branching after observation)
- **Step count** (3 -> 10+ steps)
- **Tool type** (file ops, code search, shell commands)

Example tasks:

1. Read a file, find a function, rename it across the repo.
2. Search for a bug pattern, fix it, run tests.
3. Create a new module, wire it into the entry point, update docs.
4. Multi-step debugging: read logs -> locate error -> patch -> verify.

Each task has a **known-correct expected outcome** (files changed, tests passing) verified independently.

### Metrics

| Metric | How to measure |
|--------|---------------|
| Total wall-clock time | `Instant::now()` around the full loop execution |
| Number of LLM calls | Counter incremented on each provider request |
| Number of tool calls | Counter incremented on each tool dispatch |
| LLM time vs tool time | Sum durations separately |
| Task success | Binary: does the final state match expected outcome? |
| Partial completion | % of expected sub-goals achieved (for failed tasks) |

### Runner Architecture

Create a new binary or integration test in `crates/llm-harness-loop` (or a dedicated `benches/` directory) that:

1. Sets up a temporary workspace with known files.
2. Runs the same task through **mode A (ReAct)** and **mode B (batch-plan)**.
3. Records timing, call counts, and success/failure per task.
4. Outputs a comparison table.

### Batch-Plan Loop Sketch

```text
1. Send system prompt + task + available tools to LLM.
   Request structured output: { choices: [{ action, params }], done: bool }
2. If done == true, return final answer.
3. Execute choices sequentially (or in parallel if independent).
4. On error or unexpected result, send tool results back to LLM for re-plan.
5. Repeat from step 2 (with accumulated history).
```

### Controls

- Same model, temperature, and max_tokens for both modes.
- Same tool set registered.
- Same workspace and starting state.
- Run each task at least 3 times to account for LLM latency variance.
- Record model name, date, and API endpoint for reproducibility.

## Success Criteria

| Criterion | Target |
|-----------|--------|
| Speed improvement (linear tasks) | >= 2x wall-clock reduction |
| Task completion (linear tasks) | >= 90% of ReAct baseline |
| Task completion (branching tasks) | Quantify gap; acceptable if >= 70% |
| Stability | Results reproducible across 3 runs |

## Next Steps

1. Define the structured-output schema for the batch plan (`choices` field).
2. Implement the batch-plan loop runner.
3. Prepare the task suite with expected outcomes.
4. Run the benchmark.
5. Write up results and decide whether to adopt this pattern in production agent flows.
