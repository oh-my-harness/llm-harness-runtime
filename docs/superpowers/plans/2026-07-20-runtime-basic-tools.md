# runtime-basic-tools Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Create `llm-harness-runtime-tools` crate providing `bash`, `read`, `write`, `edit` tools that map onto `ExecutionEnv` primitives, with hashline-anchored read↔edit coupling via a `FileSnapshotStore`.

**Architecture:** New satellite crate `crates/llm-harness-runtime-tools/` beside `runtime-mcp` etc. Depends on `llm-harness-agent` (for `Plugin`) and `llm-harness-types` (for `Tool`/`ToolContext`/`ExecutionEnv`). `FsToolsPlugin` registers all 4 tools with a shared optional `Arc<RwLock<FileSnapshotStore>>`. TDD with mock `ExecutionEnv`; integration tests use `runtime-sandbox-os`.

**Tech Stack:** Rust 2024 edition, `tokio`, `serde_json`, `sha2`, `thiserror`, `futures`, `llm-harness-types`, `llm-harness-agent`.

## Global Constraints

- Edition: `2024` (from `workspace.package.edition`).
- Version: `0.3.0` (from `workspace.package.version`).
- License: `MIT` (from `workspace.package.license`).
- Language rule: comments/docs in English or Chinese only; no Japanese/Korean/other.
- Concurrency: use `std::sync::RwLock` + snapshot pattern (lock → clone → drop → operate); never hold lock across `.await`. Matches `SkillReadTool` pattern (`Arc<RwLock<Vec<Skill>>>`).
- `pub` items must have `///` doc comments.
- Commit format: `<type>(<scope>): <description>` — scope is `tools`.
- Before each commit: `cargo fmt`, `cargo clippy --all-targets --all-features` (with `PYO3_PYTHON=/usr/local/bin/python3.14`), `cargo test -p llm-harness-runtime-tools`.
- Hashline tag: 4 hex characters (SHA-256 first 2 bytes → hex).
- Tool names: `bash`, `read`, `write`, `edit` (all `ToolLevel::User`, not `BuiltIn`).

---

## File Structure

```
crates/llm-harness-runtime-tools/
├── Cargo.toml                          # Crate manifest
└── src/
    ├── lib.rs                          # Re-exports + FsToolsPlugin + resolve_path helper
    ├── snapshot.rs                     # FileSnapshotStore + SnapshotEntry
    ├── bash.rs                         # BashTool
    ├── read.rs                         # ReadTool
    ├── write.rs                        # WriteTool
    ├── edit.rs                         # EditTool + EditOperation enum
    └── test_env.rs                     # MockExecutionEnv (#[cfg(test)])
```

**Responsibilities:**
- `snapshot.rs` — in-memory path→snapshot map; tag generation; LRU eviction.
- `bash.rs` — shell command execution via `env.execute_shell`.
- `read.rs` — file/dir reading; hashline header emission; snapshot recording.
- `write.rs` — file overwrite; parent dir creation; snapshot invalidation.
- `edit.rs` — line-level swap/insert/delete; tag verification; batch operations.
- `test_env.rs` — mock `ExecutionEnv` for unit tests (in-memory FS + fake shell).
- `lib.rs` — public API surface + `FsToolsPlugin` aggregating all tools + shared `resolve_path`.

**Workspace changes:**
- Modify root `Cargo.toml`: add `crates/llm-harness-runtime-tools` to `[workspace].members` and `[workspace.dependencies]`.

---

### Task 1: Scaffold crate and register in workspace

**Files:**
- Create: `crates/llm-harness-runtime-tools/Cargo.toml`
- Create: `crates/llm-harness-runtime-tools/src/lib.rs` + placeholder module files
- Modify: `Cargo.toml` (root workspace manifest)

**Interfaces:**
- Produces: crate `llm-harness-runtime-tools` visible to workspace; `lib.rs` with module declarations and shared `resolve_path`.

- [ ] **Step 1: Create `Cargo.toml`**

Create `crates/llm-harness-runtime-tools/Cargo.toml`:

```toml
[package]
name    = "llm-harness-runtime-tools"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
llm-harness-agent.workspace = true
llm-harness-types.workspace = true
sha2.workspace              = true
serde.workspace             = true
serde_json.workspace        = true
tokio = { workspace = true, features = ["time", "fs"] }
futures.workspace           = true
thiserror.workspace         = true
chrono.workspace            = true
uuid.workspace              = true

[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
tempfile = "3"
llm-harness-runtime-sandbox-os = { path = "../llm-harness-runtime-sandbox-os" }
```

- [ ] **Step 2: Create `src/lib.rs` with module declarations and shared helper**

Create `crates/llm-harness-runtime-tools/src/lib.rs`:

```rust
//! Out-of-the-box basic tools (`bash`, `read`, `write`, `edit`) for
//! `llm-harness-runtime`-based agents.
//!
//! All tools map onto [`llm_harness_types::ExecutionEnv`] primitives.
//! `read` and `edit` are coupled via [`FileSnapshotStore`] using
//! hashline anchors (4-hex content tags) to detect stale edits.

pub mod bash;
pub mod edit;
pub mod read;
pub mod snapshot;
pub mod write;

pub use bash::BashTool;
pub use edit::{EditOperation, EditTool};
pub use read::ReadTool;
pub use snapshot::FileSnapshotStore;
pub use write::WriteTool;

use std::path::{Path, PathBuf};

/// Resolve a path against the env working dir if relative.
pub(crate) fn resolve_path(working_dir: &Path, p: &str) -> PathBuf {
    let path = PathBuf::from(p);
    if path.is_absolute() {
        path
    } else {
        working_dir.join(path)
    }
}
```

Create placeholder files `src/bash.rs`, `src/edit.rs`, `src/read.rs`, `src/snapshot.rs`, `src/write.rs` — each containing only:

```rust
//! (placeholder — implemented in later tasks)
```

- [ ] **Step 3: Register in workspace**

Modify root `Cargo.toml`. In `[workspace].members`, add after the `llm-harness-runtime-mcp` line:

```toml
    "crates/llm-harness-runtime-tools",
```

In `[workspace.dependencies]`, add after the `llm-harness-runtime = ...` line:

```toml
llm-harness-runtime-tools = { path = "crates/llm-harness-runtime-tools" }
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check -p llm-harness-runtime-tools`
Expected: compiles with no errors.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add Cargo.toml crates/llm-harness-runtime-tools/
git commit -m "feat(tools): scaffold llm-harness-runtime-tools crate"
```

---

### Task 2: Implement `FileSnapshotStore`

**Files:**
- Modify: `crates/llm-harness-runtime-tools/src/snapshot.rs` (replace placeholder)

**Interfaces:**
- Produces:
  - `pub struct FileSnapshotStore { /* private RwLock<HashMap> */ }`
  - `impl FileSnapshotStore { pub fn new() -> Self; pub fn record(&self, path: &Path, lines: Vec<String>) -> String; pub fn lookup(&self, path: &Path) -> Option<SnapshotEntry>; pub fn invalidate(&self, path: &Path); }`
  - `pub struct SnapshotEntry { pub tag: String, pub lines: Vec<String>, pub mtime: SystemTime }`
  - `pub fn compute_tag(lines: &[String]) -> String`

- [ ] **Step 1: Write implementation with tests**

Replace `crates/llm-harness-runtime-tools/src/snapshot.rs` with the full implementation (see spec section "FileSnapshotStore"). Key elements:

```rust
//! In-memory whole-file snapshot store shared between `ReadTool` (writer)
//! and `EditTool` (reader). Uses 4-hex content-hash tags for stale detection.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;

use sha2::{Digest, Sha256};

const MAX_PATHS: usize = 30;
const SNAPSHOT_MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct SnapshotEntry {
    pub tag: String,
    pub lines: Vec<String>,
    pub mtime: SystemTime,
}

pub struct FileSnapshotStore {
    snapshots: RwLock<HashMap<PathBuf, SnapshotEntry>>,
}

impl FileSnapshotStore {
    pub fn new() -> Self {
        Self { snapshots: RwLock::new(HashMap::new()) }
    }

    pub fn record(&self, path: &Path, lines: Vec<String>) -> String {
        let total_bytes: usize = lines.iter().map(|l| l.len() + 1).sum();
        let tag = compute_tag(&lines);
        if total_bytes > SNAPSHOT_MAX_BYTES {
            return tag;
        }
        let entry = SnapshotEntry { tag: tag.clone(), lines, mtime: SystemTime::now() };
        let mut map = self.snapshots.write().unwrap();
        if map.len() >= MAX_PATHS && !map.contains_key(path) {
            if let Some(lru_key) = map.iter().min_by_key(|(_, e)| e.mtime).map(|(k, _)| k.clone()) {
                map.remove(&lru_key);
            }
        }
        map.insert(path.to_path_buf(), entry);
        tag
    }

    pub fn lookup(&self, path: &Path) -> Option<SnapshotEntry> {
        let mut map = self.snapshots.write().unwrap();
        if let Some(entry) = map.get_mut(path) {
            entry.mtime = SystemTime::now();
            return Some(entry.clone());
        }
        None
    }

    pub fn invalidate(&self, path: &Path) {
        self.snapshots.write().unwrap().remove(path);
    }
}

impl Default for FileSnapshotStore {
    fn default() -> Self { Self::new() }
}

pub fn compute_tag(lines: &[String]) -> String {
    let normalized: Vec<&str> = lines.iter().map(|l| l.trim_end_matches('\r')).collect();
    let joined = normalized.join("\n");
    let mut hasher = Sha256::new();
    hasher.update(joined.as_bytes());
    let result = hasher.finalize();
    result[..2].iter().map(|b| format!("{:02x}", b)).collect()
}
```

Tests to include in `#[cfg(test)] mod tests`:
- `tag_is_deterministic` — same input → same 4-hex tag.
- `tag_changes_on_content_change` — different content → different tag.
- `tag_ignores_trailing_cr` — `"hello"` and `"hello\r"` produce same tag.
- `record_and_lookup` — record then lookup returns entry with matching tag and lines.
- `invalidate_drops_entry` — after invalidate, lookup returns None.
- `lru_eviction_at_capacity` — fill MAX_PATHS, add one more, oldest is evicted.
- `large_file_not_snapshotted` — file > SNAPSHOT_MAX_BYTES: tag computed but not stored.

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p llm-harness-runtime-tools snapshot`
Expected: all 7 tests PASS.

- [ ] **Step 3: Commit**

```bash
cargo fmt
PYO3_PYTHON=/usr/local/bin/python3.14 cargo clippy -p llm-harness-runtime-tools --all-targets --all-features
cargo test -p llm-harness-runtime-tools
git add crates/llm-harness-runtime-tools/src/snapshot.rs
git commit -m "feat(tools): implement FileSnapshotStore with hashline tags"
```

---

### Task 3: Implement `MockExecutionEnv` test helper

**Files:**
- Create: `crates/llm-harness-runtime-tools/src/test_env.rs` (replace placeholder)
- Modify: `crates/llm-harness-runtime-tools/src/lib.rs` — add `#[cfg(test)] pub mod test_env;`

**Interfaces:**
- Produces: `pub struct MockExecutionEnv` implementing `ExecutionEnv`.
  - `pub fn new() -> Self` — empty FS, working_dir = `/tmp/mock`.
  - `pub fn write_file_sync(&self, path: &Path, content: &str)` — seed file.
  - `pub fn mkdir_sync(&self, path: &Path)` — seed directory.

- [ ] **Step 1: Implement `MockExecutionEnv`**

Create `crates/llm-harness-runtime-tools/src/test_env.rs` with `#![cfg(test)]`. In-memory `HashMap<PathBuf, Vec<u8>>` for files, `Vec<PathBuf>` for dirs, `Mutex`-protected. Working dir `/tmp/mock`. All `ExecutionEnv` methods implemented: `read_text_file`, `read_text_lines` (splits on `\n`, truncates to `max_lines`), `read_binary_file`, `write_file`, `append_file`, `file_info` (checks dirs then files, returns `NotFound` otherwise), `list_dir` (finds immediate children), `exists`, `create_dir` (pushes to dirs vec), `remove`, `create_temp_dir` (uses `uuid::Uuid::new_v4`), `execute_shell` (fake: `echo <text>` → stdout=text, `false` → exit_code=1, else exit_code=0), `cleanup` (no-op).

Key detail: `normalize()` resolves relative paths against working_dir. All methods normalize first. Locks are acquired and dropped within the async block (never held across `.await` — the mock is synchronous internally).

- [ ] **Step 2: Register module in `lib.rs`**

Add after `pub mod write;`:

```rust
#[cfg(test)]
pub mod test_env;
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo test -p llm-harness-runtime-tools --no-run`
Expected: compiles with no errors.

- [ ] **Step 4: Commit**

```bash
cargo fmt
PYO3_PYTHON=/usr/local/bin/python3.14 cargo clippy -p llm-harness-runtime-tools --all-targets --all-features
git add crates/llm-harness-runtime-tools/src/test_env.rs crates/llm-harness-runtime-tools/src/lib.rs
git commit -m "feat(tools): add MockExecutionEnv test helper"
```

---

### Task 4: Implement `WriteTool`

**Files:**
- Modify: `crates/llm-harness-runtime-tools/src/write.rs` (replace placeholder)

**Interfaces:**
- Consumes: `FileSnapshotStore` (Task 2), `resolve_path` (Task 1), `Tool`/`ToolContext`/`ToolError`/`ToolResult`/`ContentBlock` from `llm-harness-types`.
- Produces:
  - `pub struct WriteTool { store: Option<Arc<RwLock<FileSnapshotStore>>> }`
  - `impl WriteTool { pub fn new(store: Option<Arc<RwLock<FileSnapshotStore>>>) -> Self }`
  - `impl Tool for WriteTool` — name `"write"`.

- [ ] **Step 1: Write implementation with tests**

`WriteTool` schema: `{ path: string (required), content: string (required) }`.

Execute flow:
1. Parse `path` and `content` (missing → `ToolError::InvalidArguments`).
2. `resolve_path(working_dir, path_str)`.
3. If `path.parent()` exists, is non-empty, and != working_dir: `env.create_dir(parent, true, abort)`.
4. `env.write_file(path, content.as_bytes(), abort)`.
5. If store present: `store.read().unwrap().invalidate(&path)`.
6. Return `ToolResult` with text `Wrote N bytes to <path>`.

Tests:
- `write_new_file` — writes, verifies content via `env.read_text_file`.
- `write_overwrites_existing` — seeds old content, writes new, verifies new.
- `write_invalidates_snapshot` — seeds snapshot, writes, verifies snapshot is None.
- `write_missing_path_arg` — missing path → `ToolError::InvalidArguments`.

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p llm-harness-runtime-tools write`
Expected: all 4 tests PASS.

- [ ] **Step 3: Commit**

```bash
cargo fmt
PYO3_PYTHON=/usr/local/bin/python3.14 cargo clippy -p llm-harness-runtime-tools --all-targets --all-features
cargo test -p llm-harness-runtime-tools
git add crates/llm-harness-runtime-tools/src/write.rs
git commit -m "feat(tools): implement WriteTool with snapshot invalidation"
```

---

### Task 5: Implement `ReadTool`

**Files:**
- Modify: `crates/llm-harness-runtime-tools/src/read.rs` (replace placeholder)

**Interfaces:**
- Consumes: `FileSnapshotStore` (Task 2), `resolve_path` (Task 1), `Tool`/`ToolContext` from `llm-harness-types`.
- Produces:
  - `pub struct ReadTool { store: Option<Arc<RwLock<FileSnapshotStore>>> }`
  - `impl ReadTool { pub fn new(store: Option<Arc<RwLock<FileSnapshotStore>>>) -> Self }`
  - `impl Tool for ReadTool` — name `"read"`.

- [ ] **Step 1: Write implementation with tests**

`ReadTool` schema: `{ path: string (required), offset: integer (optional, 1-indexed), limit: integer (optional), raw: boolean (optional, default false) }`.

Constants: `DEFAULT_MAX_LINES = 300`, `RANGE_LEADING_CONTEXT_LINES = 1`, `RANGE_TRAILING_CONTEXT_LINES = 3`.

Execute flow:
1. Parse `path`, `offset`, `limit`, `raw`.
2. `resolve_path`.
3. `env.file_info(path)` → if `is_dir`, call `read_directory`.
4. `read_file`:
   - `env.read_text_lines(path, None)` → all lines. On `InvalidUtf8` error, return `[Binary file, N bytes]` (after `read_binary_file` to get size).
   - If all_lines empty but file has bytes (binary with no newlines), same binary notice.
   - `effective_limit = limit.unwrap_or(DEFAULT_MAX_LINES)`.
   - `start = offset.unwrap_or(1) - 1` (0-indexed).
   - `end = (start + effective_limit).min(total)`.
   - If offset/limit given: expand `display_start = start.saturating_sub(1)`, `display_end = (end + 3).min(total)`. Else use start/end.
   - Slice `all_lines[display_start..display_end]`.
   - If `!raw` and store present: `tag = store.record(path, all_lines.clone())`.
   - `format_output(path, &sliced, display_start, raw, tag_opt)`:
     - raw: `lines.join("\n")`.
     - tag present: `[PATH#TAG]\n` + `LINE:TEXT` numbered (1-indexed from `display_start + 1`).
     - no tag: `LINE:TEXT` numbered only (no header).

`read_directory`:
- `env.list_dir(path)`.
- Empty → `(empty directory)`.
- Each entry: `name` + `/` if dir, ` (N bytes)` if file with size > 0.
- Sort, truncate to 12, append `... (truncated)` if needed.

Tests:
- `read_file_hashline_mode` — store present, verifies `[PATH#TAG]` header, `LINE:TEXT` rows, snapshot recorded.
- `read_file_raw_mode` — `raw: true`, no header, no snapshot, plain text.
- `read_file_no_store_numbered` — store None, still numbered lines, no header.
- `read_file_with_offset_limit` — offset=5, limit=2 on 10-line file, verifies context lines (4-9).
- `read_directory` — two files, both listed.
- `read_empty_directory` — returns `(empty directory)`.
- `read_nonexistent_file` — `ToolError::Execution`.

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p llm-harness-runtime-tools read`
Expected: all 7 tests PASS.

- [ ] **Step 3: Commit**

```bash
cargo fmt
PYO3_PYTHON=/usr/local/bin/python3.14 cargo clippy -p llm-harness-runtime-tools --all-targets --all-features
cargo test -p llm-harness-runtime-tools
git add crates/llm-harness-runtime-tools/src/read.rs
git commit -m "feat(tools): implement ReadTool with hashline anchoring"
```

---

### Task 6: Implement `EditTool`

**Files:**
- Modify: `crates/llm-harness-runtime-tools/src/edit.rs` (replace placeholder)

**Interfaces:**
- Consumes: `FileSnapshotStore` (Task 2), `resolve_path` (Task 1), `Tool`/`ToolContext` from `llm-harness-types`.
- Produces:
  - `pub enum EditOperation { Swap { start, end, lines }, Insert { start, lines }, Delete { start, end } }` — `#[serde(tag = "type", rename_all = "snake_case")]`.
  - `pub struct EditTool { store: Option<Arc<RwLock<FileSnapshotStore>>> }`
  - `impl EditTool { pub fn new(store: Option<Arc<RwLock<FileSnapshotStore>>>) -> Self }`
  - `impl Tool for EditTool` — name `"edit"`.

- [ ] **Step 1: Write implementation with tests**

`EditTool` schema: `{ path: string (required), tag: string (optional), operations: array (required) }`. Each operation: `{ type: "swap"|"insert"|"delete", start: integer, end: integer (swap/delete), lines: string[] (swap/insert) }`.

Execute flow:
1. Parse `path`, `tag`, `operations` (deserialize via `serde_json::from_value`).
2. `resolve_path`.
3. Acquire base lines:
   - If `tag` and store present: `store.lookup(path)`.
     - Entry exists: if `entry.tag != tag` → `ToolError::Execution("stale snapshot: ...")`. Else use `entry.lines`.
     - No entry (invalidated by write): fall back to `env.read_text_lines(path, None)`.
   - Else: `env.read_text_lines(path, None)`.
4. Apply operations in sequence. Line numbers 1-indexed, refer to state after prior ops.
   - `Swap { start, end, lines }`: validate `start >= 1 && end >= start && end <= len`. `lines.splice(start-1..end, new_lines)`.
   - `Insert { start, lines }`: validate `start >= 1 && start <= len + 1`. Insert each line at `start-1 + i`.
   - `Delete { start, end }`: validate same as swap. `lines.drain(start-1..end)`.
5. `env.write_file(path, content.as_bytes(), abort)` where content = `lines.join("\n") + "\n"`.
6. If store present: `store.record(path, lines.clone())` (refresh tag).
7. Return `Edited <path>: N operation(s) applied, M lines → K lines`.

Tests:
- `edit_swap_single` — swap line 2 → verifies content.
- `edit_insert` — insert before line 2.
- `edit_delete` — delete line 2.
- `edit_multiple_operations` — delete(1,1) + insert(1,Z) + swap(3,3,D) on 4-line file.
- `edit_tag_match_succeeds` — record snapshot, edit with matching tag succeeds.
- `edit_tag_mismatch_errors` — edit with wrong tag → error contains "stale".
- `edit_no_store_no_tag_works` — store None, no tag, edits from env.
- `edit_out_of_bounds_errors` — delete range beyond file length.
- `edit_after_write_falls_back_to_env` — record, invalidate, edit with old tag → succeeds (falls back).

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p llm-harness-runtime-tools edit`
Expected: all 9 tests PASS.

- [ ] **Step 3: Commit**

```bash
cargo fmt
PYO3_PYTHON=/usr/local/bin/python3.14 cargo clippy -p llm-harness-runtime-tools --all-targets --all-features
cargo test -p llm-harness-runtime-tools
git add crates/llm-harness-runtime-tools/src/edit.rs
git commit -m "feat(tools): implement EditTool with tag verification"
```

---

### Task 7: Implement `BashTool`

**Files:**
- Modify: `crates/llm-harness-runtime-tools/src/bash.rs` (replace placeholder)

**Interfaces:**
- Consumes: `resolve_path` (Task 1), `Tool`/`ToolContext` from `llm-harness-types`, `ShellOptions`/`ShellOutput` from `llm-harness-types`.
- Produces:
  - `pub struct BashTool` (no store needed)
  - `impl BashTool { pub fn new() -> Self }`
  - `impl Tool for BashTool` — name `"bash"`.

- [ ] **Step 1: Write implementation with tests**

`BashTool` schema: `{ command: string (required), cwd: string (optional), timeout: integer (optional, 1..3600, default 300), env: object (optional, string→string) }`.

Constants: `DEFAULT_TIMEOUT_SECS = 300`, `MIN_TIMEOUT_SECS = 1`, `MAX_TIMEOUT_SECS = 3600`.

Execute flow:
1. Parse `command` (required), `cwd`, `timeout`, `env`.
2. Clamp timeout: `timeout.unwrap_or(DEFAULT_TIMEOUT_SECS).clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS)`.
3. Resolve cwd via `resolve_path` if provided.
4. Build `env_vec: Vec<(&str, &str)>` from the env object.
5. Construct `ShellOptions { cwd: cwd.as_deref(), env: env_vec, timeout: Some(Duration::from_secs(timeout)), abort: ctx.abort.clone(), on_stdout: None, on_stderr: None }`.
6. `ctx.env.execute_shell(command, opts)`.
7. Merge stdout + stderr (stdout first, then stderr if non-empty).
8. Exit code 0: return `ToolResult` with output text (or `(no output)` if empty).
9. Non-zero exit: return `ToolResult { is_error: true (via details or content), ... }` with output + `\nCommand exited with code <n>`. **Note**: `ToolResult` has no `is_error` field — use `ContentBlock::Text` with the error text and set `details` to `{ "exit_code": N }`. The `is_error` semantics are conveyed by including `Command exited with code <n>` in the text.
10. `EnvError::ShellFailed` → convert to `ToolResult` with error text (not `ToolError`).
11. `EnvError::Aborted` → `ToolError::Aborted`.

Tests (using `MockExecutionEnv` whose fake shell handles `echo` and `false`):
- `bash_echo` — `echo hello` → stdout `hello`, exit 0.
- `bash_nonzero_exit` — `false` → output with `Command exited with code 1`.
- `bash_no_output` — empty command → `(no output)`.
- `bash_missing_command_arg` — missing command → `ToolError::InvalidArguments`.

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p llm-harness-runtime-tools bash`
Expected: all 4 tests PASS.

- [ ] **Step 3: Commit**

```bash
cargo fmt
PYO3_PYTHON=/usr/local/bin/python3.14 cargo clippy -p llm-harness-runtime-tools --all-targets --all-features
cargo test -p llm-harness-runtime-tools
git add crates/llm-harness-runtime-tools/src/bash.rs
git commit -m "feat(tools): implement BashTool"
```

---

### Task 8: Implement `FsToolsPlugin` and integration test

**Files:**
- Modify: `crates/llm-harness-runtime-tools/src/lib.rs` — add `FsToolsPlugin`.

**Interfaces:**
- Consumes: all 4 tools (Tasks 4-7), `FileSnapshotStore` (Task 2), `Plugin` from `llm-harness-agent`.
- Produces:
  - `pub struct FsToolsPlugin { store: Option<Arc<RwLock<FileSnapshotStore>>> }`
  - `impl FsToolsPlugin { pub fn new(store: Option<Arc<RwLock<FileSnapshotStore>>>) -> Self }`
  - `impl Plugin for FsToolsPlugin` — name `"fs-tools"`, registers all 4 tools.

- [ ] **Step 1: Add `FsToolsPlugin` to `lib.rs`**

Append to `crates/llm-harness-runtime-tools/src/lib.rs`:

```rust
use std::sync::{Arc, RwLock};

use llm_harness_agent::Plugin;
use llm_harness_types::Tool;

/// Plugin that registers `bash`, `read`, `write`, `edit` tools with an
/// optional shared `FileSnapshotStore` coupling `read` and `edit`.
pub struct FsToolsPlugin {
    store: Option<Arc<RwLock<FileSnapshotStore>>>,
}

impl FsToolsPlugin {
    pub fn new(store: Option<Arc<RwLock<FileSnapshotStore>>>) -> Self {
        Self { store }
    }
}

impl Plugin for FsToolsPlugin {
    fn name(&self) -> &str {
        "fs-tools"
    }

    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        tools.push(Arc::new(ReadTool::new(self.store.clone())));
        tools.push(Arc::new(WriteTool::new(self.store.clone())));
        tools.push(Arc::new(EditTool::new(self.store.clone())));
        tools.push(Arc::new(BashTool::new()));
    }
}
```

- [ ] **Step 2: Write integration test**

Add `#[cfg(test)] mod tests` at the bottom of `lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::Tool;

    #[test]
    fn plugin_registers_four_tools() {
        let plugin = FsToolsPlugin::new(None);
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        plugin.register_tools(&mut tools);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"read"));
        assert!(names.contains(&"write"));
        assert!(names.contains(&"edit"));
        assert!(names.contains(&"bash"));
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn plugin_name() {
        let plugin = FsToolsPlugin::new(None);
        assert_eq!(plugin.name(), "fs-tools");
    }
}
```

- [ ] **Step 3: Run all tests**

Run: `cargo test -p llm-harness-runtime-tools`
Expected: all tests across all modules PASS.

- [ ] **Step 4: Commit**

```bash
cargo fmt
PYO3_PYTHON=/usr/local/bin/python3.14 cargo clippy -p llm-harness-runtime-tools --all-targets --all-features
cargo test -p llm-harness-runtime-tools
git add crates/llm-harness-runtime-tools/src/lib.rs
git commit -m "feat(tools): implement FsToolsPlugin aggregating all tools"
```

---

### Task 9: Full workspace verification and push

**Files:** None (verification only).

- [ ] **Step 1: Run full workspace test**

Run: `CARGO_HTTP_PROXY="" cargo test --workspace`
Expected: all tests pass, no regressions in other crates.

- [ ] **Step 2: Run clippy on workspace**

Run: `PYO3_PYTHON=/usr/local/bin/python3.14 cargo clippy --workspace --all-targets --all-features`
Expected: no warnings.

- [ ] **Step 3: Push**

```bash
git push origin HEAD
```

- [ ] **Step 4: Verify usage example compiles**

Create a temporary example file (do not commit) to verify the public API:

```rust
// verify_api.rs (not committed)
use llm_harness_runtime_tools::{FsToolsPlugin, FileSnapshotStore};
use std::sync::{Arc, RwLock};

fn main() {
    let store = Arc::new(RwLock::new(FileSnapshotStore::new()));
    let _plugin = FsToolsPlugin::new(Some(store));
}
```

Run: `rustc --edition 2024 --crate-type bin verify_api.rs --extern llm_harness_runtime_tools=target/debug/libllm_harness_runtime_tools.rlib -L target/debug/deps` (or use `cargo` with a scratch project). Expected: compiles.

---

## Self-Review Notes

**Spec coverage:**
- ✅ Crate `llm-harness-runtime-tools` — Task 1.
- ✅ `FileSnapshotStore` with tag, LRU, capacity limits — Task 2.
- ✅ `BashTool` — Task 7.
- ✅ `ReadTool` with hashline/raw/offset/limit — Task 5.
- ✅ `WriteTool` with snapshot invalidation — Task 4.
- ✅ `EditTool` with tag verification, operations batch — Task 6.
- ✅ `FsToolsPlugin` — Task 8.
- ✅ Mock `ExecutionEnv` — Task 3.
- ✅ Integration test (read→edit cycle) — covered in Task 6's `edit_after_write_falls_back_to_env` and Task 8's plugin registration test.

**Placeholder scan:** No TBD/TODO. All steps have concrete code or precise descriptions.

**Type consistency:** `FileSnapshotStore` methods (`record`, `lookup`, `invalidate`) are consistent across Tasks 2, 4, 5, 6. `EditOperation` enum variants (`Swap`, `Insert`, `Delete`) match between Task 6 definition and tests. `resolve_path` is in `lib.rs` (Task 1) and consumed by Tasks 4-7 via `crate::resolve_path`.
