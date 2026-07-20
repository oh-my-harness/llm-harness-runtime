# runtime-basic-tools Design

- **Date**: 2026-07-20
- **Status**: Approved
- **Scope**: New crate `llm-harness-runtime-tools` providing out-of-the-box basic tools (`bash`, `read`, `write`, `edit`) for agents built on `llm-harness-runtime`.

## Background

The `llm-harness-runtime` workspace defines a `Tool` trait (`llm-harness-types/src/tool.rs`) and an `ExecutionEnv` abstraction (`llm-harness-types/src/env.rs`) exposing filesystem + shell primitives. The runtime currently ships only framework-reserved builtin tools (`final_answer`, `spawn_agent`, `skill_read`, subagent communication tools, …) — it has **no user-facing basic tools** that an agent can call to read files, write files, edit lines, or run shell commands.

`oh-my-pi` (the TypeScript coding agent) has a rich tool catalog (`docs/tools/`), but most of its surface area (archive/sqlite/URL/internal-url/notebook for `read`; PTY/async-job/auto-background/interceptor for `bash`) depends on runtimes or native bindings not available on the Rust side. This design ports a **core functional subset** that maps directly onto `ExecutionEnv` primitives, keeping the implementation small and dependency-free.

## Goals

1. Provide a reusable, opt-in crate with the four most common coding-agent tools.
2. Map each tool onto `ExecutionEnv` primitives — no new I/O dependencies.
3. Couple `read` and `edit` via a hashline anchor scheme (content-hash tag) so edits can detect stale snapshots.
4. Integrate with `HarnessBuilder` through the existing `Plugin` trait.

## Non-Goals

- archive / sqlite / URL / internal-url / notebook / image / document-conversion support in `ReadTool`.
- PTY / async job / auto-background / command interceptor in `BashTool`.
- `grep` / `glob` standalone tools (may be added later).
- Structural code summary in `ReadTool` (may be added later).
- Stale-edit automatic recovery in `EditTool` (only error reporting).
- Full parity with `oh-my-pi` tool semantics.

## Architecture

### Crate Layout

New crate `crates/llm-harness-runtime-tools/`, a satellite alongside `runtime-mcp` / `runtime-audit-jsonl` / `runtime-trace-otel`.

```
crates/llm-harness-runtime-tools/
├── Cargo.toml
└── src/
    ├── lib.rs          # Re-exports + FsToolsPlugin
    ├── snapshot.rs     # FileSnapshotStore
    ├── bash.rs         # BashTool
    ├── read.rs         # ReadTool
    ├── write.rs        # WriteTool
    └── edit.rs         # EditTool
```

**Dependency direction** (respects existing rules):

```
llm-harness-runtime-tools → llm-harness-agent → llm-harness-loop → llm-harness-types
```

`runtime-tools` depends on `llm-harness-agent` (for `Plugin`) and `llm-harness-types` (for `Tool`, `ToolContext`, `ExecutionEnv`). It does not depend on the `llm-harness-runtime` crate itself — it only consumes the trait abstractions, matching the satellite pattern.

### Workspace Registration

Add to root `Cargo.toml`:
- `[workspace].members`: `"crates/llm-harness-runtime-tools"`
- `[workspace.dependencies]`: `llm-harness-runtime-tools = { path = "crates/llm-harness-runtime-tools" }`

### Plugin Integration

`FsToolsPlugin` implements `Plugin`. Constructed with an optional shared snapshot store:

```rust
pub struct FsToolsPlugin {
    store: Option<Arc<RwLock<FileSnapshotStore>>>,
}

impl FsToolsPlugin {
    pub fn new(store: Option<Arc<RwLock<FileSnapshotStore>>>) -> Self { ... }
}

impl Plugin for FsToolsPlugin {
    fn name(&self) -> &str { "fs-tools" }
    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        tools.push(Arc::new(ReadTool::new(self.store.clone())));
        tools.push(Arc::new(WriteTool::new(self.store.clone())));
        tools.push(Arc::new(EditTool::new(self.store.clone())));
        tools.push(Arc::new(BashTool::new()));
    }
}
```

**Usage**:

```rust
let store = Arc::new(RwLock::new(FileSnapshotStore::new()));
let harness = HarnessBuilder::new("claude-3-5-sonnet-20241022")
    .install(&FsToolsPlugin::new(Some(store)))
    .provider("claude-*", client)
    .build(env)
    .await?;
```

Users can also bypass the plugin and register tools individually via `.tool(Arc::new(ReadTool::new(store.clone())))`.

**Degradation when `store = None`**: `ReadTool` emits no hashline header; `WriteTool` skips invalidation (no-op); `EditTool` skips stale verification and edits purely by line number against the current file content. All three still function. `BashTool` is unaffected (never uses the store).

## Components

### FileSnapshotStore (`src/snapshot.rs`)

In-memory store of whole-file snapshots, shared between `ReadTool` (writer) and `EditTool` (reader).

```rust
pub struct FileSnapshotStore {
    snapshots: Mutex<HashMap<PathBuf, SnapshotEntry>>,
}

struct SnapshotEntry {
    tag: String,          // 4-hex content hash
    lines: Vec<String>,   // whole-file snapshot
    mtime: SystemTime,
}

impl FileSnapshotStore {
    pub fn new() -> Self { ... }
    pub fn record(&self, path: &Path, lines: Vec<String>) -> String; // returns tag
    pub fn lookup(&self, path: &Path) -> Option<SnapshotEntry>;
    pub fn invalidate(&self, path: &Path);
}
```

**Tag generation**: SHA-256 over normalized content (strip trailing `\r`, join with `\n`), take first 2 bytes → 4 hex characters. Matches `oh-my-pi`'s 4-hex tag format.

**Capacity limits** (hardcoded constants, aligned with `oh-my-pi`):

| Constant | Value | Meaning |
|---|---|---|
| `MAX_PATHS` | 30 | Max distinct paths retained |
| `SNAPSHOT_MAX_BYTES` | 4 MiB | Files larger than this are not snapshotted |
| `MAX_VERSIONS_PER_PATH` | 1 | Only the latest snapshot per path |

Eviction: LRU by last access. A new snapshot for an existing path replaces the previous one (single version).

### BashTool (`src/bash.rs`)

**Input schema**:

```json
{
  "command": "string (required)",
  "cwd": "string (optional) — relative to env.working_dir()",
  "timeout": "number (optional) — seconds, default 300, clamped 1..3600",
  "env": "object (optional) — extra env vars"
}
```

**Behavior**:
1. Parse args; clamp `timeout` to `[1, 3600]`, default `300`.
2. Resolve `cwd` against `env.working_dir()` (if provided).
3. Call `ctx.env.execute_shell(command, ShellOptions { cwd, timeout, env, abort: ctx.abort.clone() })`.
4. Merge stdout + stderr.
5. Exit code 0 → normal `ToolResult` with output text (or `(no output)` if empty).
6. Non-zero exit → `ToolResult { is_error: true, ... }` with output + `Command exited with code <n>` suffix. Not thrown.
7. Timeout / abort → `ToolError` with captured output (if any).

**Constants**: `DEFAULT_TIMEOUT = 300`, `MIN_TIMEOUT = 1`, `MAX_TIMEOUT = 3600`.

**Not implemented**: PTY, async jobs, auto-background, interceptor, internal-URL expansion.

### ReadTool (`src/read.rs`)

**Input schema**:

{
  "path": "string (required) — file or directory path",
  "offset": "number (optional) — 1-indexed start line",
  "limit": "number (optional) — line count",
  "raw": "boolean (optional) — verbatim mode, suppresses hashline header and line prefixes; default false"
}
```

**Behavior**:
1. Resolve path against `env.working_dir()`.
2. `env.file_info(path)` to determine type.
3. **Directory**: `env.list_dir()`. Render a flat listing (depth 1, max 12 entries per dir). Empty dir → `(empty directory)`. Each entry: `name` + ` (size)` when size > 0, trailing `/` for directories.
4. **File**:
   - `env.read_text_lines(path, Some(limit))`.
   - No `offset`/`limit`: read first 300 lines (`DEFAULT_MAX_LINES`).
   - With `offset`/`limit`: read `[offset, offset + limit - 1]`, add 1 leading + 3 trailing context lines on constrained sides (constants `RANGE_LEADING_CONTEXT_LINES = 1`, `RANGE_TRAILING_CONTEXT_LINES = 3`).
   - **Hashline mode** (store present and `raw != true`): output `[PATH#TAG]` header followed by `LINE:TEXT` rows. Call `store.record(path, lines)` to store snapshot.
   - **Raw mode** (`raw == true` or store absent): plain text, no line prefix, no snapshot.
   - Binary (non-UTF-8) file: `env.read_binary_file` to get size, return `[Binary file, N bytes]` text.
5. Out-of-bounds line ranges do not throw — return text with a suggestion (`Use :1 ...` or `Use :<last> ...`).

**Constants**: `DEFAULT_MAX_LINES = 300`, `READ_CHUNK_SIZE = 8192`.

**Not implemented**: archive, sqlite, URL, internal-url, notebook, image, document conversion, structural summary.

### WriteTool (`src/write.rs`)

**Input schema**:

```json
{
  "path": "string (required)",
  "content": "string (required)"
}
```

**Behavior**:
1. Resolve path against `env.working_dir()`.
2. Create parent directories: `env.create_dir(parent, true)`.
3. `env.write_file(path, content.as_bytes(), abort)`.
4. Return `Wrote N bytes to <path>`.
5. If store present: `store.invalidate(path)` to drop any stale snapshot, forcing the next `read` to re-snapshot. This prevents `EditTool` from matching a tag against an outdated snapshot after the file was overwritten.

**Not implemented**: append mode, archive writes.

### EditTool (`src/edit.rs`)

**Input schema**:

```json
{
  "path": "string (required)",
  "tag": "string (optional) — hashline tag for stale verification",
  "operations": [
    {
      "type": "\"swap\" | \"insert\" | \"delete\"",
      "start": "number — 1-indexed start line",
      "end": "number — inclusive end line (swap/delete only)",
      "lines": "string[] — new content (swap/insert only)"
    }
  ]
}
```

**Behavior**:
1. Resolve path against `env.working_dir()`.
2. Acquire base lines:
   - If `tag` provided and store present: `store.lookup(path)`. If entry exists and tag matches → use snapshot lines as base. If tag mismatches → `ToolError("stale snapshot: tag <given> does not match <actual>; re-read the file")`. If no entry → fall back to `env.read_text_lines(path, None)`.
   - No `tag` or no store: `env.read_text_lines(path, None)`.
3. Apply `operations` in sequence against the in-memory line vector. Line numbers refer to the state after prior operations in the same batch.
   - `swap`: replace lines `[start, end]` with `lines`.
   - `insert`: insert `lines` before line `start`.
   - `delete`: remove lines `[start, end]`.
4. Validate line numbers against current vector length. Out-of-range → `ToolError`.
5. `env.write_file(path, joined_content.as_bytes(), abort)`.
6. If store present: `store.record(path, new_lines)` to refresh tag.
7. Return summary: `Edited <path>: N operation(s) applied, M lines → K lines`.

**Not implemented**: stale-edit auto-recovery, partial-apply rollback on mid-batch error.

## Data Flow

```mermaid
sequenceDiagram
    participant LLM
    participant Loop
    participant RT as ReadTool
    participant Store as FileSnapshotStore
    participant Env as ExecutionEnv
    participant ET as EditTool

    LLM->>Loop: read(path)
    Loop->>RT: execute
    RT->>Env: read_text_lines(path)
    RT->>Store: record(path, lines) → tag
    RT-->>LLM: [PATH#TAG] + LINE:TEXT

    LLM->>Loop: edit(path, tag, ops)
    Loop->>ET: execute
    ET->>Store: lookup(path) → entry
    Note over ET: verify tag == entry.tag
    ET->>ET: apply ops to entry.lines
    ET->>Env: write_file(path, content)
    ET->>Store: record(path, new_lines) → new tag
    ET-->>LLM: edited N ops
```

## Error Handling

All tools return `Result<ToolResult, ToolError>`; no panics.

| Scenario | Tool | Handling |
|---|---|---|
| Path does not exist | read, edit | `ToolError` |
| Path is directory, file expected | read, edit | `ToolError` |
| Binary file (non-UTF-8) | read | Text notice, not an error |
| Line range out of bounds | edit | `ToolError` with valid range hint |
| Tag mismatch (stale) | edit | `ToolError`, prompt to re-read |
| Non-zero shell exit | bash | `ToolResult { is_error: true }` (not thrown) |
| Shell timeout / abort | bash | `ToolError` with captured output |
| Parent dir creation failure | write | `ToolError`, propagate `EnvError` |
| Write failure | write, edit | `ToolError`, propagate `EnvError` |

## Configuration

Beyond `FsToolsPlugin::new(store: Option<...>)`, no configuration surface. Constants (timeouts, line limits, snapshot capacity) are hardcoded. YAGNI: make configurable only when a real need arises.

## Testing

TDD per CLAUDE.md. Mock `ExecutionEnv` in `#[cfg(test)]` (following the `MockTool` pattern in `builder.rs`); no real OS filesystem.

1. **`FileSnapshotStore`**: tag determinism, LRU eviction, capacity limits, large-file skip.
2. **`ReadTool`**: directory render, full-file read, offset/limit, binary notice, hashline header + store write, raw-mode suppression.
3. **`WriteTool`**: new file, overwrite, auto parent-dir creation, store invalidation on overwrite.
4. **`EditTool`**: swap/insert/delete single-op, multi-op batch, tag-match edit, tag-mismatch error, no-store degradation, out-of-bounds error.
5. **`BashTool`**: normal output, non-zero exit (`is_error`), timeout, env passthrough.
6. **Integration**: read → write → edit (stale tag) → re-read → edit cycle verifying snapshot invalidation; `FsToolsPlugin` registers all 4 tools.

## Design Decisions

- **WriteTool invalidates the snapshot store** on every write. Without this, a `write` between `read` and `edit` would leave a stale snapshot whose tag still matches the LLM-provided tag, causing `EditTool` to apply operations against outdated line content. Invalidation forces the next `read` to re-snapshot, and `EditTool` falls back to `env.read_text_lines` when the store has no entry for the path (if the LLM calls `edit` with the old tag after `write`, `store.lookup` returns `None` and the edit proceeds against the current file content without tag verification — acceptable degraded behavior, since the LLM was instructed to re-read).
- **4-hex tag (2 bytes)** matches `oh-my-pi`'s format exactly, keeping cross-tool anchor compatibility.
- **`raw` as a schema field** rather than a path suffix (`:raw`) simplifies the Rust implementation — no path/suffix parser needed. The LLM passes `raw: true` explicitly.
- **No structural code summary in `ReadTool`** for the initial implementation. The full line-numbered output is sufficient; summary can be added later without schema changes.
- **EditTool reports stale but does not auto-recover**. Auto-recovery requires re-reading and re-anchoring, which is complex; YAGNI for now.
