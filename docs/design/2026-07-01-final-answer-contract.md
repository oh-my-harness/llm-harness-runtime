# Final Answer Event Contract

## Status

Implemented as a backward-compatible runtime protocol extension.

## Problem

The runtime already emits low-level stream events such as `TextDelta`,
`MessageEnd`, `ToolCallStart`, `ToolExecutionEnd`, `TurnEnd`, and `AgentEnd`.
Those events are useful for debugging and live streaming, but product UIs
should not infer durable assistant answers from raw text deltas.

Without an explicit final-answer contract, intermediate assistant text before
tool calls can be restored as normal assistant bubbles. For example, a model may
emit "I will check that first", call a tool, then produce the real answer in a
later turn. The first text is progress, not the final user-visible answer.

## Contract

`AssistantMessage` now carries display semantics and stable runtime identity:

- `kind`: `progress`, `reasoning_summary`, or `final_answer`.
- `message_id`: stable ID for the assistant message when produced by the loop.
- `turn_id`: stable ID for the user turn that produced the assistant message.

Old session JSON that does not contain `kind` still deserializes as
`final_answer`; old messages without IDs remain valid.

The loop also emits structured answer events:

- `AgentEvent::Progress { turn_id, message_id, text }`
- `AgentEvent::FinalAnswer { turn_id, message_id, text }`

Consumers should render durable assistant bubbles from `FinalAnswer` or from
persisted assistant messages whose `kind` is `final_answer`. They may show
`Progress` in a live progress area, but should not restore it as the final
assistant answer.

## Runtime Semantics

- Assistant messages ending with `StopReason::ToolUse` are marked as
  `progress`.
- Assistant messages ending without tool use are marked as `final_answer`.
- `FinalAnswer.text` is built from visible text blocks only; thinking and
  tool-use blocks are excluded.
- `MessageEnd`, `TurnEnd`, `TextDelta`, and tool events remain available for
  existing subscribers and debugging.
- `AgentHarness` persists the classified `AssistantMessage`, so sessions can
  distinguish progress from final answers after reload.
- Workflow LLM steps now prefer `FinalAnswer` for `StepResult.output` and fall
  back to raw text deltas only when no final-answer event is available.

## Remaining Optional Extension

An opt-in `answer` or `final_answer` tool can be added later for products that
want to force a model to call a specific tool before a turn is accepted as
final. That should be configurable, because making it mandatory would change
prompting and tool-choice behavior for existing integrations.
