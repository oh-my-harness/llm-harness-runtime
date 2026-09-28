# Local Decision-Model Benchmark

## Current Experiment Progress

Updated: 2026-09-28, Asia/Shanghai.

- [x] Define a task-independent observation-only rule policy.
- [x] Remove the absolute workspace prompt and repeat cloud/local ReAct runs.
- [x] Measure rules plus large model under the same corrected prompt and validators.
- [x] Compare quality, false completion, calls, latency and rule overhead; record limitations.

Progress log:
- 2026-09-28: Started the next experiment. Rules may inventory/read bounded files
  and read back successful writes, but cannot inspect task expectations or
  determine completion. Small-model routing and broader tasks remain later
  experiments, contingent on evidence from this control.
- 2026-09-28: Completed 27 measured tasks across corrected-prompt large/small
  ReAct and rules plus large. Six regression tests passed. Results below retain
  failures and distinguish successful-task latency from all-attempt latency.

Research issue: https://github.com/oh-my-harness/llm-harness-runtime/issues/259

## Question

Can a local small model reduce full agent-task latency while preserving task
success, compared with cloud Qwen2.5-72B? The previous simulated 8-13x speedup
does not establish real model performance. JSON-object mode also does not
implement a trained finite-choice decision model or a large-model escalation
policy.

## Environment

- Windows, RTX 5070 Ti, 16303 MiB VRAM, NVIDIA driver 591.86.
- Node 24.14.1, OpenAI JavaScript SDK pinned to 7.23.0.
- Ollama portable 0.34.4 downloaded from the official GitHub release.
- Archive SHA256: `535193f38f3344e5b08f5d1c171c31ce11aa17f0124ff69ae26d8ec7fe06fa62`.
- Local model: `qwen3.5:4b`, Q4_K_M, metadata parameter size 4.7B.
- Model digest: `2a654d98e6fba55d452b7043684e9b57a947e393bbffa62485a7aac05ee4eefd`.
- Ollama reports model size and GPU-resident size both 3,128,038,521 bytes,
  context length 4096. CUDA detected the RTX 5070 Ti.
- Ollama files live in the sibling `benchmark/tools/ollama` directory; models
  belong in `benchmark/models`, outside the source repository.

Official references:
[Windows portable distribution](https://docs.ollama.com/windows),
[OpenAI-compatible endpoint](https://docs.ollama.com/api/openai-compatibility),
[Qwen3.5 model catalog](https://ollama.com/library/qwen3.5).

## Protocol Changes

- Separate large/small endpoints and environment-based credentials.
- One separately recorded warmup per endpoint; three measured runs per task.
- Identical prompt and tools across arms; 10 steps, 256 output tokens,
  temperature zero, 60-second request timeout, no automatic SDK retries.
- File-only tools confined to a fresh temporary workspace; no shell tool.
- Rename/comment validators require preserved source content, not just a
  matching substring. CRLF and one optional final newline are normalized.
  README retains the original phrase-presence check.
- Failed API requests retain elapsed latency and earlier completed steps.
- Raw JSON is saved after each task, including decision text, tool results,
  usage, termination reason, errors, and timings; credentials are excluded.
- Warmup time is excluded from task latency. Task setup/cleanup are excluded.
- Endpoint arms run sequentially, not randomly interleaved. Network load and
  provider scheduling remain potential confounders.
- Three tiny tasks are a smoke benchmark, not evidence of general coding
  competence or production readiness. Updated tool/validator conditions make
  these runs a new baseline, not directly comparable to historical numbers.

## Reproduction

Install benchmark dependencies and run regression tests from the repository:

```powershell
npm ci --ignore-scripts
npm test
```

Start the prepared local server in a terminal (keep it open):

```powershell
./scripts/start-benchmark-ollama.ps1
```

In another terminal, download the model and inspect the installed model metadata:

```powershell
Invoke-RestMethod -Method Post http://127.0.0.1:11434/api/pull `
  -ContentType application/json -Body '{"model":"qwen3.5:4b","stream":false}' `
  -TimeoutSec 1800
Invoke-RestMethod http://127.0.0.1:11434/api/tags
```

Run the local arm; `reasoning_effort=none` requests disabled thinking through
the compatible endpoint. Verify the server accepts it and inspect traces before
interpreting results. Record model digest, quantization, and GPU residency.

```powershell
node scripts/benchmark_jev_real.mjs --mode small --small-model qwen3.5:4b `
  --small-base-url http://127.0.0.1:11434/v1 --small-thinking off `
  --runs 3 --output logs/benchmarks/local-qwen35-4b.json
```

For a two-arm run, provision `LARGE_API_KEY` securely in the process environment,
then omit `--mode small`. The large endpoint defaults to SiliconFlow and model
to `Qwen/Qwen2.5-72B-Instruct`. `SMALL_API_KEY` is optional for localhost.
Do not put credentials into commands, result files, or version control.

## Status

The initial automated background launch was rejected by execution policy.
After the user explicitly authorized startup, a retry succeeded. Ollama 0.34.4
is listening on `127.0.0.1:11434`, with cloud features disabled. The local model
download, cold-load check, and measured task runs have completed.

Cloud baseline measurement completed with schema version 2:

| Task | Success | Mean task time | Mean API call | Calls/task |
| --- | --- | --- | --- | --- |
| rename_function | 3/3 | 4.35 s | 1086 ms | 4 |
| add_comment | 3/3 | 3.14 s | 1045 ms | 3 |
| create_readme | 3/3 | 2.69 s | 894 ms | 3 |

Raw result: `logs/benchmarks/2026-09-28-cloud-72b-v2.json`. There were no format
errors in this nine-task run. This sample does not establish a reliable success
rate or local-vs-cloud speedup.

The initial diagnostic run is preserved as
`logs/benchmarks/2026-09-28-cloud-72b.json`. Its schema-1 validator mistakenly
required a final newline and therefore reported false failures. Trace inspection
also identified two genuine failures: one approximately 60-second API timeout
and one comment edit that erased `DEBUG = True`. The remaining seven outputs
satisfy the corrected content rules. Do not treat schema-1's raw success field as
the corrected success rate, or pool the two runs without explicit re-scoring.
The subsequent run did not change the prompt/tools, only validation, error-type
reporting and final-file recording. Both raw runs remain local and ignored by Git.

Validation: four HTTP-fixture regression tests passed (endpoint routing,
failure/timeout accounting, invalid decisions/path confinement, and preservation
of existing content). JavaScript and PowerShell syntax checks passed. The Rust
workspace was not modified, and its full test suite was not run for this change.

## Local Results

Raw result: `logs/benchmarks/2026-09-28-local-qwen35-4b.json`, schema version 2.
The explicit cold-load JSON probe took 61.202 seconds and returned valid JSON
without a reasoning field. The subsequent benchmark warmup took 158 ms. Cold
loading and initialization are excluded from the following task measurements.

| Task | Success | Mean task time | Mean API call | Calls/task | Cloud/local time ratio |
| --- | --- | --- | --- | --- | --- |
| rename_function | 3/3 | 0.73 s | 244 ms | 3 | 5.9x |
| add_comment | 0/3 | 0.60 s | 299 ms | 2 | Not a successful-task speedup |
| create_readme | 3/3 | 0.54 s | 181 ms | 3 | 4.9x |

All three comment-task failures followed the same pattern: the model supplied
an absolute path despite the relative-path instruction, the file tool rejected
it, and the model then declared completion without editing the file. The saved
final file remained `DEBUG = True`. JSON formatting succeeded in all runs, but
valid JSON did not imply valid actions or task completion.

On this small smoke suite, local success is 6/9 versus cloud 9/9, below the
experiment's target of 85% of baseline success. The successful task classes are
approximately 5-6x faster. This does not validate a 7-10x quality-preserving
speedup, nor does it establish a general failure rate from only three tasks.
Rename also uses fewer calls locally (3 versus 4), so its task-level speedup is
not solely a per-call latency effect; the current verifier checks final content,
not whether the requested verification tool step was performed.

The local arm requested `reasoning_effort=none`; cloud used its default. Ollama's
model defaults include presence penalty 1.5, while the benchmark specifies only
temperature zero. Quantization, inference backend, caching and unspecified
sampling defaults differ between endpoints, so this is a deployment comparison,
not an isolated experiment on parameter count. A follow-up should explicitly
control supported sampling parameters and re-run both arms.

Next experiment: remove the irrelevant absolute workspace path from both arms'
prompts (the tools require relative paths), then repeat the paired evaluation
without changing the validators. Keep these original results as the baseline;
any prompt change defines a new condition. Expand the task suite only after
checking error recovery and false completion behavior.

## Relative-Path and Generic-Observation Experiment

The next experiment above is now complete. The only prompt change in the two
ReAct arms was removal of the absolute `Workspace:` prefix. Both still receive
the same relative-path instruction, tool schema, task text and validators.
Sampling defaults and model versions remain unchanged from the preceding run.

The rules arm uses that corrected prompt with large-model generation. Its
task-independent scheduler is `scripts/benchmark_rules.mjs`:

1. List workspace files and automatically read at most eight regular files of
   at most 8192 bytes each, in stable filename order.
2. After a successful model-requested write, automatically read that path back.
3. Return all observations to the model. The model still generates changes and
   decides when to stop. The final-state evaluator is never exposed to the rules.

Rules intentionally add observations and some tool calls. They do not cache
model answers, repair output schemas, supply expected edits, or stop after a
write. Rule time includes its tool time, so it must not be added to `toolMs`.
The 10-step budget bounds model decisions; automatic observations are extra
bounded operations, recorded separately. Initial reads are metadata-bounded;
write readbacks are bounded indirectly by the model output-token budget.

### Measured Results

Three runs per task and arm, one excluded warmup per endpoint. Times below are
means over all three attempts, including failures.

| Task | Large ReAct success / time / calls | Small ReAct success / time / calls | Rules + large success / time / calls |
| --- | --- | --- | --- |
| rename_function | 3/3 / 3.81 s / 4 | 3/3 / 0.88 s / 4 | 3/3 / 2.90 s / 3 |
| add_comment | 0/3 / 2.03 s / 2 | 3/3 / 0.60 s / 3 | 3/3 / 2.04 s / 2 |
| create_readme | 3/3 / 2.31 s / 3 | 0/3 / 1.22 s / 4 | 3/3 / 1.75 s / 2 |

| Across all nine attempts | Large ReAct | Small ReAct | Rules + large |
| --- | --- | --- | --- |
| Success | 6/9 | 6/9 | 9/9 |
| False completion | 3/9 | 3/9 | 0/9 |
| Total model requests | 27 | 33 | 21 |
| Mean elapsed | 2.716 s | 0.901 s | 2.228 s |
| Median elapsed | 2.527 s | 0.875 s | 2.030 s |
| Empirical p95 elapsed | 4.007 s | 1.235 s | 2.936 s |
| Mean tool time | 1.52 ms | 1.02 ms | 1.79 ms |
| Mean rule time (including automatic tools) | 0 | 0 | 1.13 ms |

With nine samples, nearest-rank p95 is simply the maximum; it is not a robust
tail estimate. Model requests account for over 99.8% of task elapsed time in
all three arms. Calls include the model's final `done` decision.

Raw artifacts (ignored by Git):
- `logs/benchmarks/2026-09-28-relative-react.json`
- `logs/benchmarks/2026-09-28-relative-rules-large.json`

Both use schema 3 and `promptVersion=relative-paths-v2`. Reproduce with the
previous endpoint/model flags and `--policy react` (default), followed by
`--mode large --policy rules`; use unique output filenames.

### Interpretation

- Removing the absolute path fixed the small model's comment failures but did
  not improve overall success. Its README outputs placed `path` and `content`
  outside `args`; it then claimed completion after tool errors. Valid JSON is
  not schema compliance. The existing `formatErrors` counter covers JSON parse
  and unknown-action errors, not all argument-shape errors; these failures are
  visible in tool errors, traces, final files and false-completion metrics.
- Large ReAct overwrote the original config in all three comment runs. The
  rules arm supplied the existing content before the model acted and preserved
  it. This is evidence for supplying cheap observations, not evidence that the
  rules can evaluate task correctness.
- Rules plus large used 22.2% fewer model requests. All-attempt mean time fell
  18.0%, but the success distributions differ, so this is not an equal-quality
  aggregate speedup. In the two task classes both arms completed correctly,
  each saved one call and about 24% of elapsed time (about 1.32x speedup).
- The local model remains much faster per request, but it used more requests
  than the large model and had different failures. Neither direct substitution
  nor a 7-10x reliable speedup has been established.

The two rules were tested together; their individual contributions have not
been isolated. Fixed arm order, only three unique tasks, provider caching,
quantization and sampling defaults limit causal and general conclusions. The
cloud's earlier 9/9 run is a different prompt condition, not a contradiction or
a reason to discard this run. Repeated near-identical trials are not nine
independent task types.

### Research Decision

Continue investigating task-independent state handling before introducing a
small-model router. The useful demonstrated mechanism is replacing mechanical
observation decisions, not replacing semantic reasoning with canned answers.
Next, freeze a broader task suite with unseen file names/content, missing files,
recoverable tool errors and multi-file edits, then compare inventory-only,
readback-only and combined rules against ReAct in counterbalanced order. Add
explicit schema validation as a separately versioned condition shared by all
arms. Only after this stronger control should a rules + small-router + large
generator arm test the incremental value of model-based routing. This round
does not implement or claim results for that hybrid routing architecture.
