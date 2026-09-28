# Local Decision-Model Benchmark

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
- Intended local model: `qwen3.5:4b`; model download and GPU execution are pending.
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

The automated background server launch was rejected by the execution policy
with `blocked by policy`. No local model was downloaded or measured. The
foreground launcher is provided for a user-controlled startup; it was not
executed to work around the rejection.

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
