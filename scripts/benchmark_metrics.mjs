const mean = (values) => values.length ? values.reduce((sum, value) => sum + value, 0) / values.length : null;

function summarize(rows) {
  const elapsed = rows.map((row) => row.totalMs).sort((a, b) => a - b);
  const successful = rows.filter((row) => row.success);
  const failed = rows.filter((row) => !row.success);
  const sum = (key) => rows.reduce((total, row) => total + row[key], 0);
  const llmCalls = sum("llmCalls");
  const toolCalls = sum("toolCalls");
  return {
    attempts: rows.length,
    successes: successful.length,
    accuracy: rows.length ? successful.length / rows.length : null,
    falseCompletions: rows.filter((row) => row.termination === "done" && !row.success).length,
    apiFailures: rows.filter((row) => row.termination === "api_error").length,
    totalElapsedMs: sum("totalMs"),
    meanElapsedMs: mean(elapsed),
    medianElapsedMs: elapsed.length ? mean(elapsed.slice(Math.floor((elapsed.length - 1) / 2), Math.floor(elapsed.length / 2) + 1)) : null,
    p95ElapsedMs: elapsed.length ? elapsed[Math.ceil(elapsed.length * 0.95) - 1] : null,
    meanSuccessfulElapsedMs: mean(successful.map((row) => row.totalMs)),
    meanFailedElapsedMs: mean(failed.map((row) => row.totalMs)),
    llmCalls,
    meanLlmCalls: rows.length ? llmCalls / rows.length : null,
    llmMs: sum("llmMs"),
    meanLlmCallMs: llmCalls ? sum("llmMs") / llmCalls : null,
    toolCalls,
    toolMs: sum("toolMs"),
    meanToolCallMs: toolCalls ? sum("toolMs") / toolCalls : null,
  };
}

export function summarizeResults(rows) {
  const byTask = Object.groupBy(rows, (row) => row.taskId);
  return {
    overall: summarize(rows),
    byTask: Object.fromEntries(Object.entries(byTask).map(([taskId, taskRows]) => [taskId, summarize(taskRows)])),
  };
}
