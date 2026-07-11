# harness.rs Module Split Design

**Date**: 2026-07-10
**Status**: Approved
**Scope**: `crates/llm-harness-agent/src/harness.rs` → `harness/` directory module

## Background

`crates/llm-harness-agent/src/harness.rs` has grown to 4108 lines (158KB). It contains:

- Non-test code (~2134 lines): types, events, hooks, `AgentHarness` struct + impl, loop driver, compaction, phase guard
- Test code (~1974 lines): three inline test modules (`tests`, `cost_state_tests`, `cost_reset_tests`)

The file is difficult to navigate, slow to load in editors, and mixes multiple unrelated responsibilities. The project already has a precedent for directory-style module organization in `session/`, which splits into `session.rs`, `types.rs`, `storage.rs`, `tree_ops.rs`, `jsonl.rs`, `repo.rs`.

## Goal

Split `harness.rs` into a `harness/` directory of focused sub-modules, preserving all existing public APIs and behavior. Zero changes to `lib.rs` or any external consumer.

## Non-Goals

- Refactoring logic, renaming methods, or changing behavior.
- Splitting any other file in the crate.
- Changing the public API surface.

## Design

### Module Structure

Convert `harness.rs` → `harness/` directory. `lib.rs`'s `pub mod harness;` remains unchanged (Rust resolves it to `harness/mod.rs`).

#### Non-test modules (11 files)

| File | Contents | Approx lines |
|---|---|---|
| `mod.rs` | Sub-module declarations + re-exports (keeps `lib.rs`'s `pub use harness::{...}` working) | ~30 |
| `state.rs` | `HarnessState`, `HarnessHooks`, `HarnessInner` + `reset_channels`, constants (`BROADCAST_CAPACITY`, `DEFAULT_QUEUE_CAPACITY`, `DEFAULT_MAX_TOKENS`), `dedup_skills`, `dedup_templates` | ~200 |
| `event.rs` | `AgentHarnessEvent`, `CompactionStats`, `HarnessToolCallResult` | ~150 |
| `hooks.rs` | `IdleWatcherHook`, `AsyncSpawnHookHandle`, `AbortCascadeHook` | ~40 |
| `follow_up.rs` | `FollowUpHandle` | ~30 |
| `options.rs` | `AgentHarnessOptions` + `new()` | ~75 |
| `prepare_next_turn.rs` | `DefaultPrepareNextTurn` (private struct) | ~55 |
| `phase_guard.rs` | `PhaseGuard` + `enter_phase`, `set_phase`, `notify_phase_change` | ~50 |
| `core.rs` | `AgentHarness` struct definition + construction (`new_in_memory`, `with_session`) + all pub methods (structural ops, runtime config, branch ops, queue ops, observation, abort, `emit` helper) | ~700 |
| `loop_driver.rs` | `run_loop`, `drive_loop`, `build_loop_config`, `run_before_run_hook`, `push_pending_write`, `flush_pending_writes` | ~350 |
| `compaction_ops.rs` | `compact`, `try_auto_compact`, `do_compact`, `apply_compaction_result` | ~150 |

#### Test modules (9 files)

| File | Contents | Approx lines |
|---|---|---|
| `tests/mod.rs` | Shared helpers (`make_harness`, `make_harness_failing_storage`, `FailAfterNStorage`, `DummyTool`) + sub-module declarations | ~100 |
| `tests/phase_guard.rs` | `enter_phase` atomicity, guard drop restores Idle, flush error restores phase, `compact_sets_compacting_phase` | ~80 |
| `tests/lifecycle.rs` | prompt completes/returns Idle, prompt while running rejects, continue_run, abort series (cancel token, cascade hook, idle watcher, aborted event), wait_for_idle, subscribe receives events, append_message/custom_entry while turning | ~250 |
| `tests/config.rs` | set_model, set_system_prompt, set_max_tokens, set_thinking_level, set_temperature, set_final_answer_mode, set_session_name, set_active_tools, with_session rejects zero queue | ~100 |
| `tests/queues.rs` | steer, follow_up, next_turn injection, clear_all_queues, clear queues rejected when not idle, has_queued_messages, FollowUpHandle injects message into loop | ~150 |
| `tests/branches.rs` | fork_branch, navigate_tree, list_branches, delete_branch, generate_branch_summary, read_active_path, read_all_entries, branch ops reject during Turning, branch ops restore Idle | ~100 |
| `tests/compaction.rs` | compact public method, auto_compact triggers on prompt, auto_compact failure injects error, override hook, independent compaction client, compact decision forces framework compaction, invalid first kept entry | ~350 |
| `tests/tools.rs` | set_tools mid-run survives cleanup, cleanup unwraps HookedTool, set_tools in Idle no flag, mid-run preserved on LLM error, rolled back on storage error, tool name conflicts (BuiltIn vs User, reserved names, duplicates) | ~300 |
| `tests/cost.rs` | cost_state_tests + cost_reset_tests | ~90 |

### Visibility Adjustments

Directory splitting requires adjusting visibility so sibling modules can access shared internals:

1. **`HarnessInner`**: currently private `struct`. Change to `pub(super)` with all fields `pub(super)`. Sibling modules (`loop_driver`, `compaction_ops`, `phase_guard`) need read/write access to fields like `state`, `current_abort`, `idle_watcher`, etc.

2. **`AgentHarness` private fields** (`inner`, `client`, `compaction_model`, `session`, `env`, `event_tx`, `phase_tx`, `phase_rx`, `convert_to_llm`, `hooks`, `stream_options`): change to `pub(super)`. Multiple `impl AgentHarness` blocks in sibling files need field access.

3. **Private helper methods** used across modules:
   - `emit` — used by `core`, `loop_driver`, `compaction_ops`. Define in `core.rs` as `pub(super)`.
   - `push_pending_write`, `flush_pending_writes` — used by `loop_driver` and `core`. Define in `loop_driver.rs` as `pub(super)`.
   - `enter_phase`, `set_phase`, `notify_phase_change` — used by `core` (branch ops, compact) and `phase_guard`. Define in `phase_guard.rs`; `enter_phase` as `pub(super)`, `set_phase`/`notify_phase_change` as `pub(super)`.
   - `run_before_run_hook`, `build_loop_config`, `run_loop`, `drive_loop` — used only within `loop_driver`. Keep private to module.
   - `do_compact`, `apply_compaction_result`, `try_auto_compact` — used by `compaction_ops` and `core` (`try_auto_compact` called from `prompt_with_messages` and `continue_run` in `core.rs`). Mark `try_auto_compact` as `pub(super)`.

4. **`DefaultPrepareNextTurn`**: private struct, stays `pub(super)` in `prepare_next_turn.rs`. Only used by `loop_driver.rs` (`build_loop_config`).

5. **Existing `pub(crate)` fields in `HarnessState`** (`streaming_message`, `pending_tool_calls`, `pending_session_writes`, `queued_next_turn`): no change needed; `pub(crate)` is strictly broader than `pub(super)`.

### Multiple `impl` Blocks

Rust allows multiple `impl` blocks for the same type across different files within the same crate. The split uses this:

- `core.rs`: `impl AgentHarness { ... }` — construction + all pub methods + `emit`
- `loop_driver.rs`: `impl AgentHarness { ... }` — `run_loop`, `drive_loop`, `build_loop_config`, `run_before_run_hook`, `push_pending_write`, `flush_pending_writes`
- `compaction_ops.rs`: `impl AgentHarness { ... }` — `compact`, `try_auto_compact`, `do_compact`, `apply_compaction_result`
- `phase_guard.rs`: `impl AgentHarness { ... }` — `enter_phase`, `set_phase`, `notify_phase_change`

### `mod.rs` Contents

```rust
mod compaction_ops;
mod core;
mod event;
mod follow_up;
mod hooks;
mod loop_driver;
mod options;
mod phase_guard;
mod prepare_next_turn;
mod state;

#[cfg(test)]
#[cfg(feature = "test-utils")]
mod tests;

// Re-exports — keeps lib.rs `pub use harness::{...}` unchanged.
pub use core::AgentHarness;
pub use event::{AgentHarnessEvent, CompactionStats, HarnessToolCallResult};
pub use follow_up::FollowUpHandle;
pub use hooks::{AbortCascadeHook, AsyncSpawnHookHandle, IdleWatcherHook};
pub use options::AgentHarnessOptions;
pub use state::{HarnessHooks, HarnessState};
```

(Exact re-export paths adjusted during implementation to match where each type lands.)

### Test Module Organization

`tests/mod.rs` holds shared helpers used by multiple test files:

- `make_harness(responses) -> AgentHarness`
- `make_harness_failing_storage(fail_after, responses) -> AgentHarness`
- `FailAfterNStorage` (wraps `SessionStorage`, fails after N appends)
- `DummyTool` (simple tool impl for tool-related tests)
- `BuiltInLevelTool` (tool with `BuiltIn` level for name-conflict tests)

Specialized helpers like `CoordinatingShouldStop`, `OverrideCompactHook`, `BlockingTool` stay in the test file that uses them.

Each test file has `use super::*;` plus any additional imports it needs.

### `lib.rs` Impact

**Zero changes.** `pub mod harness;` resolves to `harness/mod.rs`. The `pub use harness::{...}` re-export list in `lib.rs` remains valid because `harness/mod.rs` re-exports the same public types.

### Migration Strategy

1. Create `harness/` directory alongside existing `harness.rs`.
2. Create `harness/mod.rs` with module declarations and re-exports.
3. Create each sub-module file by moving code from `harness.rs`, applying visibility adjustments (`pub(super)`).
4. Delete `harness.rs`.
5. Run `cargo test --features test-utils` to verify all tests pass.
6. Run `cargo clippy` to catch any visibility or import issues.

The migration is mechanical: no logic changes, no behavior changes. Each code block moves verbatim to its new home, with only `pub` → `pub(super)` adjustments on fields/structs/methods that need cross-module visibility.

## Testing

Verification after split:

1. `cargo build` — compiles with no errors
2. `cargo test --features test-utils` — all existing tests pass (no test logic changes)
3. `cargo clippy --features test-utils` — no new warnings
4. Confirm `lib.rs` is unchanged (`git diff lib.rs` shows no changes)

## Risk

- **Visibility errors**: `pub(super)` may be too narrow in some cases; fix by widening to `pub(crate)` if needed. Low risk — mechanical.
- **Import paths**: moving code may require adjusting `use` statements within each sub-module. Low risk — compiler catches all.
- **Test feature gating**: test modules are gated on `#[cfg(test)]` + `#[cfg(feature = "test-utils")]`. Must preserve both gates on `tests/mod.rs` and propagate to sub-modules.
