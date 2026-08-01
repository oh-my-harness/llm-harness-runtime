# Issue #97 Round-20 Progress

Updated: 2026-08-01 10:17 +08:00
Task: Repair the Round-19 production-gate defects without redesigning the child-reaper architecture.

## Plan

- [x] Replace the API boundary suite with one Windows-safe shared forge crate,
  independent field and construction probes, explicit non-`Clone`/non-`Copy`
  gates, and exact rustc JSON diagnostic checks.
- [x] Separate component-exit and reap-panic metrics, and remove registry state
  that has no behavior or observation contract.
- [x] Replace hand-forced fault tests with deterministic real-thread panic,
  epoch, panic-count, and recovery-owner assertions.
- [x] Add deterministic RAII active-thread verification for emergency-spawn
  rollback.
- [x] Run the Windows acceptance baseline, repeat the Linux-qualified subset in
  WSL, update the Round-19 status claim and #97 comment, then commit and push.

## Constraints

- Preserve the Round-19 reaper, emergency panic-isolation, handle-restoration,
  and takeover-predicate architecture.
- Keep #97, #98, and #99 open.
- Do not change the multi-agent runtime pin from `1be6859`.
- Treat the known WSL DrvFS permission-semantics failure as unrelated baseline
  evidence, not as a reason to alter runtime behavior.

## Follow-up Plan

- [x] Run platform-neutral supervisor, registry, admission, metrics, and mock
  fault tests on Windows; retain per-test Unix guards only for real Unix
  processes, `/proc`, and ECHILD semantics.
- [x] Replace every fault-test barrier and scanner spin gate with bounded
  channel or condition-variable synchronization on both participants.
- [x] Replace synthetic takeover completion with a switchable mock that returns
  a real terminal result only after all six normal components have exited.
- [x] Document the pre-1.0 `ReaperMetrics` field-addition compatibility decision
  and the exact emergency-only meaning of `reap_panics`.
- [~] Qualify Windows and WSL baselines, update the corrected #97 status comment,
  commit, and push while keeping #97/#98/#99 and the multi-agent pin unchanged.

## Progress Log

- 2026-08-01 00:55 +08:00 - Confirmed clean baseline `5ed65cd`, reproduced the
  Windows TOML path serialization failure from the current API gate, verified
  the per-field false-negative construction pattern, and fetched the open #97
  status history. No repository `STATUS.md` exists; the Round-19 progress issue
  comment is the status record that must be corrected after validation.
- 2026-08-01 01:10 +08:00 - Replaced ten independent API tests with one forge
  crate and target directory. JSON string serialization now produces a valid
  TOML dependency path on canonical Windows `\\?\` paths. The positive control
  and 18 per-method, per-field, construction, and trait probes pass on Windows
  in 18.81 seconds using `cargo check --offline`; negative results require an
  exact rustc code, matching target, symbol, and primary source span.
- 2026-08-01 01:10 +08:00 - Split `component_failures` from public
  `reap_panics`, removed unused per-child panic state, and made the first real
  required-component exit degrade the supervisor and advance its epoch. New
  shared traces and synchronization prove the panicking component and recovery owner.
  A true `AlwaysPanic` child drives four workers plus stuck/orphan to exit
  before emergency takeover. Emergency-spawn rollback now observes six RAII
  thread entries, six exits, and zero active threads before returning `Err`.
  WSL sandbox-os passed 70/70 serially; strict sandbox-os Clippy passes on both
  Windows and Linux.
- 2026-08-01 01:18 +08:00 - The then-current format, strict workspace Clippy,
  and workspace tests returned zero on Windows, but later review showed that
  the sandbox-os binary ran only 15 tests because the full reaper module was
  Unix-gated. This result was not evidence of Windows reaper coverage and is
  superseded by the 10:17 follow-up below. WSL passed sandbox-os 70/70, the
  consolidated API gate 1/1, and real bwrap 49/49. The WSL DrvFS workspace run
  reproduced only the declared unrelated permission-semantics failure
  `load_skill_unreadable_file_produces_diagnostic` (189/190 in that test
  binary). The five new fault/rollback tests each passed three additional
  explicit serial repetitions (15/15 executions).
- 2026-08-01 01:18 +08:00 - Mutation checks proved the former false negatives
  are closed. Making only `ChildLease.confirmed` public failed specifically at
  `child_lease_confirmed`; deriving only `Clone` for `SpawnAdmission` failed at
  `spawn_admission_clone`. Both temporary mutations were restored before final
  validation.
- 2026-08-01 01:20 +08:00 - Committed and pushed the validated implementation
  to `main` as `0822af1`. #97, #98, and #99 remain open, and no multi-agent
  dependency revision was changed.
- 2026-08-01 10:02 +08:00 - Independent follow-up accepted: Round-20's API
  gate, metric split, and real worker/epoch direction remain unchanged. Found
  that the module-level Unix guard excludes all 55 reaper tests on Windows even
  though only eight real-process tests and three ECHILD tests require Unix.
  Also confirmed unbounded barriers/scanner gating and the synthetic
  `CheckoutGuard::consume` in the takeover helper. Started a test-gate-only
  follow-up plus public metric documentation; no reaper redesign is planned.
- 2026-08-01 10:17 +08:00 - Removed the module-level Unix gate. Windows now
  lists and executes 59 sandbox-os unit tests: the prior 15 plus 44
  platform-neutral reaper tests. Eleven tests retain per-test Unix guards for
  real processes, `/proc`, or ECHILD semantics. Windows sandbox-os passed
  59/59 serially, API passed 1/1, strict workspace Clippy and format passed,
  and a full workspace rerun passed. One earlier workspace run transiently
  failed `abort_does_not_affect_future_spawns`; its isolated rerun, full test
  binary rerun, and subsequent workspace rerun all passed. No related code was
  changed.
- 2026-08-01 10:17 +08:00 - Replaced fault-test barriers and scanner spinning
  with deadline-bounded channels and a timed condition-variable gate. The
  takeover mock now panics until all six normal components have exited, then
  returns a real terminal status for the emergency thread to consume through
  the production path. No test directly manufactures terminal completion.
  `ReaperMetrics` now documents both the accepted pre-1.0 field-addition break
  and the emergency-caught-only meaning of `reap_panics`.
- 2026-08-01 10:17 +08:00 - WSL Ubuntu 24.04 (`rustc`/`cargo` 1.97.1) retained
  all 70 sandbox-os tests and passed 70/70 serially. The API gate passed 1/1,
  real bwrap passed 49/49 with ignored integration tests included, and Linux
  format plus strict workspace Clippy passed. The known DrvFS workspace
  permission-semantics baseline was not altered or used to justify code changes.
