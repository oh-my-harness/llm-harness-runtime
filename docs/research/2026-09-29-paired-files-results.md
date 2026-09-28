# Large vs Small Model: 20-Task Paired Experiment

Date: 2026-09-29, Asia/Shanghai.

## Outcome

On this fixed synthetic file-task suite, both deployments passed 57/60 attempts
(95%). The local small model was 4.05x faster on the 54 matched attempts both
models completed correctly. This supports faster local execution for these
tasks, not a general equivalence claim about coding quality.

| Metric | Cloud Qwen2.5-72B | Local Qwen3.5-4B |
| --- | --- | --- |
| Correct attempts | 57/60 (95%) | 57/60 (95%) |
| Task types passed in all repetitions | 19/20 | 19/20 |
| Mean task time, all attempts | 3.452 s | 0.831 s |
| Median task time | 3.240 s | 0.790 s |
| Empirical p95 task time | 5.404 s | 1.198 s |
| Total task elapsed, all attempts | 207.127 s | 49.888 s |
| Model requests | 210 | 207 |
| Mean requests per attempt | 3.50 | 3.45 |
| Weighted mean request latency | 985.60 ms | 240.31 ms |
| Tool calls | 150 | 147 |
| Total tool time | 95.60 ms | 89.84 ms |
| API failures | 0 | 0 |
| Failed attempts ending in `done` | 3 | 3 |

For matched successful pairs only: 54 pairs, mean large time 3.454 s versus
small time 0.853 s, ratio 4.047. The remaining pairs are three large-only
successes and three small-only successes; none failed for both. The all-attempt
mean ratio is about 4.15, but includes failures and is not the primary successful
speedup claim. Model requests account for over 99.7% of measured task time in
both arms; tool time is negligible in this particular suite.

## Per-Task Results

Times are means across three attempts, including failures; calls include the
final model `done` action. Each arm sees identical initial files.

| Task | Large correct | Large seconds | Large calls | Small correct | Small seconds | Small calls |
| --- | --- | --- | --- | --- | --- | --- |
| rename_function | 3/3 | 3.43 | 3 | 3/3 | 0.88 | 3 |
| prepend_comment | 3/3 | 3.14 | 3 | 3/3 | 0.72 | 3 |
| create_readme | 3/3 | 1.70 | 2 | 3/3 | 0.50 | 2 |
| lookup_port | 3/3 | 2.73 | 3 | 3/3 | 0.68 | 3 |
| sum_csv | 3/3 | 2.68 | 3 | 3/3 | 0.72 | 3 |
| filter_json | 3/3 | 2.74 | 3 | 3/3 | 0.76 | 3 |
| sort_unique | 3/3 | 3.03 | 3 | 3/3 | 0.73 | 3 |
| toggle_json | 3/3 | 3.14 | 3 | 3/3 | 0.73 | 3 |
| nested_json | 3/3 | 3.29 | 3 | 3/3 | 0.86 | 3 |
| section_ini | 3/3 | 3.63 | 3 | 3/3 | 0.83 | 3 |
| append_changelog | 0/3 | 2.54 | 2 | 3/3 | 0.84 | 3 |
| targeted_replace | 3/3 | 3.18 | 3 | 3/3 | 0.80 | 3 |
| rename_two_files | 3/3 | 4.84 | 5 | 3/3 | 1.25 | 5 |
| sync_version | 3/3 | 5.69 | 6 | 3/3 | 1.33 | 6 |
| split_lines | 3/3 | 3.94 | 4 | 3/3 | 0.91 | 4 |
| merge_config | 3/3 | 3.78 | 4 | 3/3 | 0.92 | 4 |
| missing_fallback | 3/3 | 3.28 | 4 | 3/3 | 0.89 | 4 |
| transient_read | 3/3 | 4.34 | 4 | 0/3 | 0.43 | 2 |
| transient_write | 3/3 | 4.27 | 5 | 3/3 | 0.92 | 5 |
| conditional_update | 3/3 | 3.71 | 4 | 3/3 | 0.95 | 4 |

## Failure Analysis

- Large model: all three `append_changelog` attempts overwrote the original
  changelog with only the new line, then incorrectly claimed the old content
  was preserved. The validator correctly rejected the lost content.
- Small model: all three `transient_read` attempts encountered the injected
  one-shot read error and returned `done` asking the user to try again, instead
  of retrying the operation. No output file was created. This is premature
  termination, not a textual claim of successful completion.
- The existing JSON field `falseCompletions` counts `done` plus failed validation.
  It therefore includes both cases above; it does not distinguish an explicit
  admission of failure from an incorrect claim of success.
- Both models recovered from the injected write failure and missing-file
  fallback in every attempt. No JSON parse or unknown-action errors occurred.

## Protocol and Provenance

- Inference code commit: `a668a68ba64a42398c91a9b4d8a4a3cf6f00d818`, pushed before
  measurement. Prompt/task definitions and validators were not tuned during runs.
- Task file: `scripts/benchmarks/tasks-v1.json`, version `paired-files-v1`.
- Recorded suite-byte SHA256:
  `586c089ae75b5cb56d5c9526eb6beefc5b8404beda90659b4a4050c367a4a91d`.
- Twenty tasks, three repetitions, two models: 120 measured attempts. Each pair
  runs sequentially; first-model order alternates by task and repetition. Each
  model runs first in 30 of the 60 pairs. Task order itself is fixed.
- Same ReAct prompt, tools, 10-step limit, 256 output-token limit, temperature
  zero and 60-second request timeout. No SDK retries. Rules/routing are absent.
- Fault recovery injects exactly one read or write error in the designated task
  for each model/attempt. The scheduler does not retry for the model. All six
  intended injections per model occurred.
- Only prompts, initial workspace content and tool results reach model context.
  Expected files are used by the offline evaluator after the loop ends.
- Validators require the exact expected file set, normalized text content or
  parsed JSON values. They check collateral changes. They do not run Python
  code, and exact-text tasks may reject alternative but semantically valid edits.
- Setup, cleanup and warmup are outside task timing. Warmup requests were
  0.652 s cloud and 3.894 s local; these are observed initialization requests,
  not a controlled measurement of fresh machine/server cold startup.
- Local server: Ollama 0.34.4, RTX 5070 Ti, Q4_K_M, 4096 context, thinking off.
  Model digest `2a654d98e6fba55d452b7043684e9b57a947e393bbffa62485a7aac05ee4eefd`.
  GPU residency was verified. Cloud precision and server defaults remain unknown.

## Data and Reproduction

The checked-in [measurement artifact](data/2026-09-29-paired-files-v1.json)
contains overall/per-task/per-category metrics, each attempt's timings and
counts, failed final files, warmups, and the source JSON hash. Full local traces
are in ignored `logs/benchmarks/2026-09-29-paired-files-v1.json`.

With the local server running and `LARGE_API_KEY` securely provisioned:

```powershell
node scripts/benchmark_jev_real.mjs --suite expanded --mode both --runs 3 `
  --small-model qwen3.5:4b --small-base-url http://127.0.0.1:11434/v1 `
  --small-thinking off --output logs/benchmarks/paired-files-new-run.json
node scripts/analyze_agent_benchmark.mjs `
  logs/benchmarks/paired-files-new-run.json `
  logs/benchmarks/paired-files-new-run-summary.json
```

Nine offline tests pass, including validator mutations, paired order, fault
recovery, failure-inclusive metrics and successful-pair speed calculations.

## Scope of the Conclusion

The experiment answers speed and accuracy for these 20 short, hand-authored
tasks. Three repeats do not create 60 independent task types. They show stable
behavior here, not a confidence interval for general coding capability.
This suite is not a standard coding benchmark or a formal held-out dataset.
Quantization, server defaults, thinking settings, network and caching differ;
the result compares deployments, not only model size. A 100 ms request latency
or 7-10x speedup was not observed: mean local request latency was 240 ms.

The next useful validation is a fresh, frozen set of longer repository tasks
with execution-based tests and more recovery cases. Keep this run as-is and
report its 95%/95% result with the different failure profiles, rather than
claiming the two models are interchangeable.
