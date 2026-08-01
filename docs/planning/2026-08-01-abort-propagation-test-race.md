# Abort Propagation Test Race Progress

Updated: 2026-08-01 10:54 +08:00
Task: Remove the test-only race between sub-agent slot reservation and actual BlockingTool entry.

## Plan

- [x] Audit all three abort propagation tests and define a per-test tool-entry
  synchronization point without changing production abort behavior.
- [x] Inject a per-test entry signal into `BlockingTool`, wait with a bounded
  timeout before each abort, and replace fixed session directories with
  `tempfile::TempDir`.
- [x] Run formatting, strict Clippy, focused high-repetition tests, and the
  Windows parallel workspace suite.
- [x] Commit and push the isolated test reliability fix with a clean worktree.

## Constraints

- Do not change reaper architecture or production abort lifecycle logic.
- Do not use added sleeps or enlarged timeouts to hide ordering races.
- Treat `in_flight` as capacity/accounting state, not proof of tool execution.

## Progress Log

- 2026-08-01 10:44 +08:00 - Confirmed the reported race. `spawn_async`
  reserves the in-flight slot before session creation and task launch, while
  the shared `MockLlmClient` consumes responses in call order. All three abort
  tests currently lack a deterministic proof that the first sub-agent reached
  `BlockingTool` before cancellation.
- 2026-08-01 10:47 +08:00 - The first signal-backed run exposed a second mock
  ordering assumption in `abort_does_not_affect_future_spawns`: both main-agent
  spawn responses were adjacent, so the first prompt could dispatch both before
  the test regained control (`in_flight == 2`). Added explicit text responses
  between rounds so only the second prompt can consume the second spawn action.
- 2026-08-01 10:47 +08:00 - Added one `Notify` and one `TempDir` per test. Each
  abort now follows a bounded wait for `BlockingTool::execute` entry; all three
  tests pass together with default parallel test execution. No production file,
  timeout value, or sleep duration changed.
- 2026-08-01 10:53 +08:00 - Windows validation passed: the target race test
  passed 50/50 repetitions; the complete abort propagation binary passed 20/20
  default-parallel repetitions (60 test executions); format and strict
  workspace Clippy passed; and the full default-parallel workspace suite passed
  3/3 runs in 59.11, 57.05, and 56.94 seconds.
- 2026-08-01 10:54 +08:00 - Committed and pushed the isolated test fix to
  `main` as `4cf9fb2`. No production, reaper, or API boundary file changed.
