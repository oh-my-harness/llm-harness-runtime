// Only observable file metadata and successful writes enter this scheduler.
// It cannot inspect the task, model answers, or the final-state evaluator.
export function nextObservation(state) {
  if (!state.initialized) {
    state.initialized = true;
    state.pending.push(
      { action: "list_files", args: {}, reason: "initial_inventory" },
      ...state.files.filter((file) => file.size <= 8192).sort((a, b) => a.path.localeCompare(b.path)).slice(0, 8)
        .map((file) => ({ action: "read_file", args: { path: file.path }, reason: "initial_read" })),
    );
  }
  return state.pending.shift();
}
