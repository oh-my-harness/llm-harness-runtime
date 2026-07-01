# Workflow Issue Closure Plan

Updated: 2026-07-01 21:50 Asia/Shanghai
Task: Close the highest-impact workflow runtime issues from the latest AI review.

## Plan

- [x] Create and maintain this plan document.
- [x] Fix pause/resume state-machine conflict (#31).
- [x] Add pause/resume/cancel end-to-end coverage (#27/#32).
- [x] Fix structured step-history propagation into downstream LLM prompts (#26/#33).
- [x] Commit, push, and update issues.
- [x] Fix terminal transition crash recovery and remove Fail double persist (#22/#23).
- [x] Commit, push, and update #22/#23.
- [x] Auto-select the declarative edge judge for expression-based workflows (#24).
- [x] Commit, push, and update #24.

## Priority Rationale

The first production risk is the pause/resume API: `resume()` currently changes
`Paused` to `Running` without driving the loop, so a caller that then invokes
`run()` can enter the crash-recovery path instead of the normal paused-task
continuation path. This is a correctness issue, not just an ergonomics issue.

After that, pause/cancel behavior needs explicit end-to-end tests so future
state-machine changes do not regress. Finally, workflow prompt context should
carry structured step results before output previews, because downstream LLM
steps need reliable data rather than truncated prose.

Lower-priority items (#24/#25/#28/#29/#30) are either documented tradeoffs,
workflow expressiveness improvements, or validation/documentation refinements.

The next correctness risk is terminal transition recovery. A persisted
`Running` state whose last completed record is `Fail` or `Abort` should converge
to the corresponding terminal task state after restore, rather than re-running
the already-completed step.

After correctness fixes, the lowest-risk usability improvement is #24: when a
workflow declares `EdgeCondition::Expr` edges and the caller supplies the default
`NoopJudge`, the engine should use `EdgeConditionJudge` automatically instead
of silently ignoring declarative routing.

## Progress Log

- 2026-07-01 20:55 Asia/Shanghai - Created plan and selected #31 as the first implementation target.
- 2026-07-01 21:02 Asia/Shanghai - Started #31 implementation; selected compatible fix where `resume()` no longer pre-sets `Running` and `run()` remains the driver.
- 2026-07-01 21:08 Asia/Shanghai - Fixed pause ordering so `current_step` advances before pausing, added typed `TaskError::Paused`, and passed targeted workflow engine tests.
- 2026-07-01 21:13 Asia/Shanghai - Added cancel restore coverage and moved to structured step-history prompt propagation.
- 2026-07-01 21:14 Asia/Shanghai - Added structured step-history prompt coverage and passed targeted workflow engine tests.
- 2026-07-01 21:16 Asia/Shanghai - Passed `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`.
- 2026-07-01 21:20 Asia/Shanghai - Pushed the workflow branch and closed GitHub issues #26, #27, #31, #32, and #33.
- 2026-07-01 21:25 Asia/Shanghai - Started #22/#23; added terminal transition recovery logic and regression tests.
- 2026-07-01 21:30 Asia/Shanghai - Passed targeted workflow tests, `cargo test --workspace`, and `cargo clippy --workspace --all-targets -- -D warnings` for #22/#23.
- 2026-07-01 21:34 Asia/Shanghai - Pushed terminal recovery fix and closed GitHub issues #22 and #23.
- 2026-07-01 21:40 Asia/Shanghai - Started #24 default declarative judge wiring.
- 2026-07-01 21:47 Asia/Shanghai - Added default declarative judge wiring for new and restored workflows; passed workflow tests, `cargo test --workspace`, and `cargo clippy --workspace --all-targets -- -D warnings`.
- 2026-07-01 21:50 Asia/Shanghai - Pushed default declarative judge fix and closed GitHub issue #24.
