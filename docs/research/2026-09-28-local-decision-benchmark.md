# Large vs Small Model Agent-Loop Experiment

Updated: 2026-09-29, Asia/Shanghai.
Research issue: https://github.com/oh-my-harness/llm-harness-runtime/issues/259

## Objective

Measure whether a local small model completes the same agent tasks faster than
a cloud large model, and how task accuracy changes. Both models use the same
sequential ReAct loop, prompts, tools, task inputs, budgets and final-state
validators. The two experiment arms are:

- Large: SiliconFlow `Qwen/Qwen2.5-72B-Instruct`.
- Small: local Ollama `qwen3.5:4b` on RTX 5070 Ti.

Primary outcomes are elapsed task time and independently verified task success.
Model/tool call counts and timings explain the observed speed. A model claiming
completion is not evidence of success. Faster failure is not successful speedup.
This compares deployment options, including network and inference backends; it
does not isolate parameter count as the cause of a difference.

## Environment

- Windows, RTX 5070 Ti, 16303 MiB VRAM, driver 591.86.
- Node 24.14.1, OpenAI JavaScript SDK 7.23.0, Ollama 0.34.4.
- Local model: Q4_K_M, metadata parameter size 4.7B, context length 4096.
- Model digest: `2a654d98e6fba55d452b7043684e9b57a947e393bbffa62485a7aac05ee4eefd`.
- Ollama reported model and GPU-resident size both 3,128,038,521 bytes.
- Cloud inference precision is unknown. Local thinking is explicitly disabled;
  cloud thinking uses its default. Other backend sampling defaults may differ.
- Portable server and models are in sibling `benchmark/tools/ollama` and
  `benchmark/models`. Service address: `127.0.0.1:11434`.

## Measurement Contract

- Accuracy = independently passed attempts / all measured attempts, including
  timeouts, API failures, exhausted budgets and incorrect results in the denominator.
- Record every task's elapsed time, cumulative model/tool time, individual call
  latencies, model/tool call counts, steps, termination reason, errors, final
  files, and model/tool trace. Model calls include failed requests and `done`.
- Count false completion when the model returns `done` but final-state validation fails.
- Persist overall and per-task summaries: accuracy, total/mean/median/p95 elapsed,
  success-only and failure-only mean elapsed, request counts and weighted mean
  per-call latency. Undefined means are null, not zero.
- All-attempt elapsed includes failures. Success-only means can have different
  task mixes, so compare paired task classes with success rates alongside speed.
- Warmup is timed separately; setup and cleanup are excluded from task timing.
  A previous local cold-load probe took 61.202 seconds; do not mix it into warm
  task latency or assume this is the cold-start time on every launch.
- No hidden SDK retries. Request timeout 60 seconds, 10 model steps, 256 output
  tokens, temperature zero, one warmup and three measured repetitions per task.
- Fresh temporary workspace per attempt. Tools are read/write/list/grep; all
  paths are relative and confined to that workspace. No shell execution.
- Rename/comment validators preserve original content and normalize CRLF and one
  optional final newline. README validation checks the required phrase.
- `formatErrors` measures JSON parsing and unknown actions, not full argument
  schema compliance. Tool errors, trace and final-state validation capture the
  currently observed malformed argument cases.

Schema 4 adds persisted summaries and per-tool-call latencies. Existing schema-3
two-model results below are retained without rewriting raw measurements; their
aggregate tool timings are available but individual tool timings were not saved.
Prompt version remains `relative-paths-v2`; metric changes do not change prompts.

## Latest Comparable Results

Source: `logs/benchmarks/2026-09-28-relative-react.json`, 3 tasks x 3 repetitions
per model, identical relative-path prompt. Times include failed attempts.

| Task | Large accuracy | Large mean time | Large calls/attempt | Small accuracy | Small mean time | Small calls/attempt |
| --- | --- | --- | --- | --- | --- | --- |
| rename_function | 3/3 | 3.81 s | 4 | 3/3 | 0.88 s | 4 |
| add_comment | 0/3 | 2.03 s | 2 | 3/3 | 0.60 s | 3 |
| create_readme | 3/3 | 2.31 s | 3 | 0/3 | 1.22 s | 4 |

| Metric, across nine attempts | Large | Small |
| --- | --- | --- |
| Accuracy | 6/9 (66.7%) | 6/9 (66.7%) |
| False completion | 3/9 | 3/9 |
| Total model calls | 27 | 33 |
| Total tool calls | 18 | 24 |
| Total task elapsed | 24.446 s | 8.113 s |
| Weighted mean model-call time | 904.69 ms | 245.43 ms |
| Mean elapsed | 2.716 s | 0.901 s |
| Median elapsed | 2.527 s | 0.875 s |
| Empirical p95 elapsed | 4.007 s | 1.235 s |
| Mean tool time per task | 1.52 ms | 1.02 ms |

Machine-readable summaries derived from the unchanged raw record are saved in
`logs/benchmarks/2026-09-28-relative-react-summary.json`. This is re-aggregation
of existing measurements, not a new inference run.

Large-model comment edits erased existing content. Small-model README calls
placed arguments outside `args`, failed, and then falsely claimed completion.
Only rename succeeded for both arms in this run; on that task the small model
was about 4.3x faster. Equal aggregate accuracy does not mean equal capability,
since different tasks failed. Do not describe the overall 3x time ratio as
quality-preserving speedup.

These are three task types, not nine independent task types. Nearest-rank p95
with nine samples equals the maximum. Fixed arm order, caching, deployment and
sampling differences remain limitations. This smoke suite cannot establish
general coding accuracy or production readiness.

## Earlier Measurements

The following local files use an earlier prompt and must not be pooled with the
latest run:

- `2026-09-28-cloud-72b-v2.json`: cloud 9/9, mean task times 4.35/3.14/2.69 s.
- `2026-09-28-local-qwen35-4b.json`: local 6/9, mean times 0.73/0.60/0.54 s;
  comment failures used absolute paths and falsely claimed completion.
- `2026-09-28-cloud-72b.json`: diagnostic schema-1 run with a faulty final-newline
  validator. Its success fields are not valid corrected scores. Trace inspection
  also found one genuine timeout and one destructive comment edit.

All raw files are under ignored `logs/benchmarks/`; credentials are never part
of reports or commits. Simulated timings are not evidence for the real-model
speed or accuracy question.

## Reproduction

```powershell
npm ci --ignore-scripts
npm test
# Start only if the local server is not already running:
./scripts/start-benchmark-ollama.ps1
```

In another terminal, with `LARGE_API_KEY` securely provisioned in the environment:

```powershell
node scripts/benchmark_jev_real.mjs --mode both --runs 3 `
  --small-model qwen3.5:4b --small-base-url http://127.0.0.1:11434/v1 `
  --small-thinking off --output logs/benchmarks/paired-new-run.json
```

Use a unique output path. Localhost needs no real API credential. For small-only
checks use `--mode small`. Read `summary.large` and `summary.small` in the JSON
for overall and per-task metrics; retain `results` for inspection and re-scoring.

## Progress

- [x] Deploy local model and run the same ReAct loop against both endpoints.
- [x] Record paired smoke results, latency, calls, accuracy and false completion.
- [x] Persist explicit overall/per-task summaries and individual tool timings.
- [x] Expand the frozen task suite to 20 tasks with independently tested validators.
- [~] Run 60 attempts per model in alternating paired order and assess speed versus accuracy.

Progress log:
- 2026-09-28: Completed initial and corrected-prompt large/small runs.
- 2026-09-28: User clarified the scope as two-model speed and quality comparison;
  consolidated documentation and measurement definitions accordingly.
- 2026-09-29: Added `paired-files-v1`, with input extraction, edits, multi-file
  consistency, missing-file fallback and one-shot read/write fault recovery.
  Expected outputs are confined to the offline evaluator. Both arms receive
  the same inputs and fault schedule. Suite hash and all 120 scheduled attempts
  are recorded. Prompt and limits are frozen before inference.
