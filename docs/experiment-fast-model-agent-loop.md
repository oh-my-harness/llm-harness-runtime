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
