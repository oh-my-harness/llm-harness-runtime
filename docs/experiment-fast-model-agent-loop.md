# Experiment: Jev-Style Fast Decision Model in Agent Loop

## Goal

Benchmark an agent loop where per-step decisions (which tool to call next) are made by a **fast structured-decision model** (Jev-style, ~70-500ms) instead of a traditional large LLM (~1-3s), and compare wall-clock speed and task completion quality.

## Hypothesis

The agent loop remains **sequential ReAct** (observe -> decide -> act -> observe). The only change is that per-step decisions are made by a **decision-only model** that outputs structured choices (not free-text). Because the decision model:
- Receives program state (conversation history, tool results)
- Returns a pre-defined choice (e.g. which tool to call next, with parameters)
- Has ~100ms latency vs ~2s for a large LLM

The total loop time should drop significantly (2-10x) for tasks with many steps, while maintaining comparable task completion for routine decisions. Complex reasoning steps are escalated to the large model only when needed.

## Architecture: System 1 / System 2

```
┌─────────────────────────────────────────────┐
│                 Agent Loop                   │
│                                              │
│   observe() ──> decide() ──> act()           │
│                    │                         │
│              ┌─────┴─────┐                   │
│              │           │                   │
│         Jev/Decision   Large LLM             │
│         (~0.1s)        (~2s)                │
│         Choice/Bool    Reasoning/Generation  │
└─────────────────────────────────────────────┘
```

- **Decision model (System 1)**: handles high-frequency, low-latency structural judgments
- **Large model (System 2)**: handles complex reasoning, planning, open-ended generation

## Two Modes Under Test

| Aspect | Large LLM ReAct (baseline) | Decision-Model ReAct (experimental) |
|--------|----------------------------|--------------------------------------|
| Per-step decision | Large LLM (~2s) | Decision model (~0.1s) |
| Output | Free-form tool call or JSON | Structured: choice/action/score/bool |
| Adaptive to errors | Yes | Yes (same loop, just faster decisions) |
| Complex reasoning | Same model handles all | Escalated to large model when needed |
| Expected speed | Baseline | ~2-10x faster |
| Expected quality | Baseline | May degrade on tasks requiring deep reasoning at every step |

## Experiment Design

### Task Suite

10-20 tasks in a sandboxed repo. Vary in reasoning complexity and step count.

### Decision Provider Interface

```rust
trait DecisionProvider {
    /// Given the current state, returns a structured decision.
    async fn decide(&self, state: &AgentState, options: &[DecisionOption]) -> Decision;
}
```

Two implementations:
- **LargeLlmDecision**: calls the large model with structured output (baseline)
- **FastDecision**: calls a small/fast model or Jev API with pre-defined options

### Model Candidates

| Role | Provider | Latency/call |
|------|----------|-------------|
| Baseline (large) | SiliconFlow Qwen-72B / GPT-4o | ~1-3s |
| Fast decision | Jev API / local small model with structured output | ~0.05-0.2s |

> Note: Jev (TypeSafe AI) is not yet available in mainland China. Use a local small model with structured output as a proxy, or APUS fast-browser-use as an open-source alternative.

### Metrics

| Metric | How to measure |
|--------|---------------|
| Total wall-clock time | `Instant::now()` around the full loop |
| Per-step decision latency | Timestamp each decision call |
| Per-step tool latency | Timestamp each tool dispatch |
| Decision model calls vs LLM calls | Separate counters |
| Task success | Binary: final state matches expected outcome? |
| Escalation rate | % of steps that required large-model fallback |

### Controls

- Same loop logic, tools, workspace for both modes.
- Only the decision model changes.
- Run each task >= 3 times.
- Record model name, endpoint, date.

## Success Criteria

| Criterion | Target |
|-----------|--------|
| Speed improvement | >= 2x wall-clock reduction |
| Task completion | >= 85% of baseline |
| Stability | Reproducible across 3 runs |

## Next Steps

1. Implement the `DecisionProvider` trait and benchmark runner.
2. Prepare task suite.
3. Run and collect results.
4. Decide whether to adopt decision-model routing in production.

---

## Benchmark Results (2026-09-28, SiliconFlow)

| Task | Baseline Qwen2.5-72B | Fast Qwen3.5-4B | Finding |
|------|---------------------|-----------------|---------|
| rename_function | 100% success, 5.8s, 5 LLM calls | 100% success, 106s, 9 LLM calls | Small model makes more errors, wastes turns on retries |
| add_comment | partial, 11.8s | partial, 35s | Same pattern: more steps, more time |
| create_readme | 100% success, 2.9s, 3 LLM calls | 100% success, 36s, 6.7 LLM calls | 12x slower despite smaller model |

### Key Finding

**Using a general-purpose small model (4B) as a replacement for a large model in the agent loop is counterproductive.** The small model:

1. Has higher per-step decision error rate (tries `cat`, `sed`, etc. on Windows)
2. Requires more steps (5 -> 9) to complete the same task
3. Results in longer total wall-clock time despite potentially faster per-call latency

### Implication for Jev-Style Architecture

The real Jev value proposition is NOT "use a smaller model" but rather:

- **Pre-defined choice options**: Jev doesn't generate free-form tool calls; it selects from a menu the developer has designed
- **Specialized training**: Jev is trained for structured decision-making, not general chat
- **No free-form text generation**: eliminates format errors and irrelevant commands

A general small LLM (Qwen3.5-4B) still generates free-form tool calls and arguments, which leads to more mistakes. A Jev-style model would constrain outputs to valid options, potentially eliminating the error-retry overhead that makes small general models slower.

### Next Steps

1. Test with Qwen3.5-9B or 14B to find the quality-speed sweet spot
2. Experiment with pre-defined choice menus instead of free-form tool selection
3. Consider implementing a mock "decision provider" that constrains outputs to valid choices