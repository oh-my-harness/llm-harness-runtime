# MCP 服务器版本锁定与最小权限凭证管理 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实现 MCP 服务器版本锁定（server_info + 工具定义快照双重校验）和最小权限凭证管理（引用式凭证 + 服务器级作用域 + 工具白名单），拒绝静默更新。

**Architecture:** 机制层在 `llm-harness-runtime-mcp` 新增 `version_lock.rs`（锁文件读写 + diff）和 `credential.rs`（引用解析 + CredentialProvider trait + 作用域映射），修改 `config.rs`（新增 VersionPolicy + credentials 字段）、`manager.rs`（连接路径插入校验 + tools/list_changed 回调改造）、`lib.rs`（McpToolAdapter 暴露 authorized_credentials）。策略层在 `llm-harness-strategy` 新增 `credential_guard.rs`（BeforeToolCallHook 默认告警实现）。

**Tech Stack:** Rust, sha2 (hash), tokio::fs (锁文件持久化), serde (config 反序列化), futures (CredentialProvider trait async), parking_lot::RwLock (并发)

**Spec:** `docs/superpowers/specs/2026-08-06-mcp-version-lock-credential-scope-design.md`

## Global Constraints

- 锁文件只存 hash，不存完整工具定义（`McpToolCache` 已负责工具缓存）
- 凭证值不落盘——只存在于 transport 运行时内存
- 默认 fail-closed：版本不匹配或工具定义变更时默认拒绝
- `credentials.scoped_env` 与 `env` 同 key 互斥（validate 时检查）
- 纯文本 env/headers 值向后兼容（无引用语法的值原样使用）
- `vault://` 引用未注入 CredentialProvider 时 fail-closed 拒绝连接
- 凭证作用域检查默认告警（不 Deny），Deny 留给用户自定义 hook

---

## File Structure

| 文件 | 职责 | 操作 |
|------|------|------|
| `crates/llm-harness-runtime-mcp/src/version_lock.rs` | 锁文件数据结构、读写、版本比对、工具定义 diff | 新建 |
| `crates/llm-harness-runtime-mcp/src/credential.rs` | CredentialProvider trait、EnvCredentialProvider、CredentialResolver、作用域映射 | 新建 |
| `crates/llm-harness-runtime-mcp/src/config.rs` | McpServerConfig 新增 VersionPolicy + credentials 字段 | 修改 |
| `crates/llm-harness-runtime-mcp/src/manager.rs` | ManagerInner 新增字段、连接路径插入校验、tools/list_changed 回调改造 | 修改 |
| `crates/llm-harness-runtime-mcp/src/lib.rs` | McpToolAdapter 新增 authorized_credentials 字段 + 数据通路 | 修改 |
| `crates/llm-harness-runtime-mcp/Cargo.toml` | 新增 sha2 依赖 | 修改 |
| `crates/llm-harness-strategy/src/credential_guard.rs` | McpCredentialGuardHook（BeforeToolCallHook 默认告警） | 新建 |
| `crates/llm-harness-strategy/src/lib.rs` | pub mod credential_guard | 修改 |

---

### Task 1: 锁文件数据结构与 hash 计算

**Files:**
- Create: `crates/llm-harness-runtime-mcp/src/version_lock.rs`
- Modify: `crates/llm-harness-runtime-mcp/Cargo.toml`（新增 sha2 依赖）
- Modify: `crates/llm-harness-runtime-mcp/src/lib.rs`（新增 `pub mod version_lock;`）
- Test: `crates/llm-harness-runtime-mcp/src/version_lock.rs`（内联 `#[cfg(test)] mod tests`）

**Interfaces:**
- Produces: `McpLockFile`（锁文件根结构）、`ServerLockEntry`（单服务器锁条目）、`ToolSnapshotEntry`（单工具 hash 快照）、`compute_tool_hashes(&[McpToolDefinition]) -> Vec<ToolSnapshotEntry>`、`diff_tool_snapshots(&[ToolSnapshotEntry], &[McpToolDefinition]) -> ToolDiff`

- [ ] **Step 1: 添加 sha2 依赖到 Cargo.toml**

在 `crates/llm-harness-runtime-mcp/Cargo.toml` 的 `[dependencies]` 末尾添加：

```toml
sha2.workspace                 = true
```

- [ ] **Step 2: 在 lib.rs 添加 module 声明**

在 `crates/llm-harness-runtime-mcp/src/lib.rs` 第 8 行 `mod utils;` 之前添加：

```rust
pub mod version_lock;
```

- [ ] **Step 3: 写失败测试——hash 计算与 diff**

在 `crates/llm-harness-runtime-mcp/src/version_lock.rs` 写入完整文件内容。注意 `sha256_hex` 使用 `format!("{:x}", hasher.finalize())` 而非 `hex::encode`，因为 `hex` crate 不一定在依赖中。`Sha256::finalize()` 返回 `GenericArray<u8, U32>`，实现了 `core::fmt::LowerHex`。

```rust
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::McpToolDefinition;

/// 锁文件根结构。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpLockFile {
    pub version: u32,
    pub servers: std::collections::HashMap<String, ServerLockEntry>,
}

/// 单个服务器的锁条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerLockEntry {
    pub server_info: ServerInfoSnapshot,
    pub protocol_version: String,
    pub tools_snapshot: Vec<ToolSnapshotEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfoSnapshot {
    pub name: String,
    pub version: String,
}

/// 单个工具的 hash 快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSnapshotEntry {
    pub name: String,
    pub description_hash: String,
    pub input_schema_hash: String,
}

/// 工具定义变更 diff 结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub modified_desc: Vec<String>,
    pub modified_schema: Vec<String>,
}

impl ToolDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.modified_desc.is_empty()
            && self.modified_schema.is_empty()
    }

    pub fn summary(&self) -> String {
        format!(
            "added: {:?}, removed: {:?}, modified_desc: {:?}, modified_schema: {:?}",
            self.added, self.removed, self.modified_desc, self.modified_schema
        )
    }
}

fn sha256_hex(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

/// 为一组工具定义计算 hash 快照。
pub fn compute_tool_hashes(tools: &[McpToolDefinition]) -> Vec<ToolSnapshotEntry> {
    tools
        .iter()
        .map(|tool| ToolSnapshotEntry {
            name: tool.name.clone(),
            description_hash: sha256_hex(&tool.description),
            input_schema_hash: sha256_hex(&tool.input_schema.to_string()),
        })
        .collect()
}

/// 比对锁文件快照与当前工具定义，返回 diff。
pub fn diff_tool_snapshots(
    locked: &[ToolSnapshotEntry],
    current: &[McpToolDefinition],
) -> ToolDiff {
    let current_hashes: std::collections::HashMap<&str, &ToolSnapshotEntry> = {
        let computed = compute_tool_hashes(current);
        computed.iter().map(|e| (e.name.as_str(), e)).collect()
    };
    let locked_map: std::collections::HashMap<&str, &ToolSnapshotEntry> =
        locked.iter().map(|e| (e.name.as_str(), e)).collect();

    let mut diff = ToolDiff::default();
    for entry in locked {
        match current_hashes.get(entry.name.as_str()) {
            None => diff.removed.push(entry.name.clone()),
            Some(current) => {
                if current.description_hash != entry.description_hash {
                    diff.modified_desc.push(entry.name.clone());
                }
                if current.input_schema_hash != entry.input_schema_hash {
                    diff.modified_schema.push(entry.name.clone());
                }
            }
        }
    }
    for entry in current_hashes.values() {
        if !locked_map.contains_key(entry.name.as_str()) {
            diff.added.push(entry.name.clone());
        }
    }
    diff
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, desc: &str) -> McpToolDefinition {
        McpToolDefinition {
            name: name.to_owned(),
            title: None,
            description: desc.to_owned(),
            input_schema: json!({"type": "object"}),
            output_schema: None,
            annotations: None,
            icons: Vec::new(),
            metadata: None,
        }
    }

    #[test]
    fn compute_tool_hashes_produces_stable_hashes() {
        let tools = vec![tool("search", "search repos")];
        let hashes = compute_tool_hashes(&tools);
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0].name, "search");
        assert!(hashes[0].description_hash.starts_with("sha256:"));
        assert!(hashes[0].input_schema_hash.starts_with("sha256:"));
    }

    #[test]
    fn diff_detects_added_tool() {
        let locked = compute_tool_hashes(&[tool("a", "desc")]);
        let current = vec![tool("a", "desc"), tool("b", "desc")];
        let diff = diff_tool_snapshots(&locked, &current);
        assert_eq!(diff.added, vec!["b"]);
        assert!(diff.removed.is_empty());
        assert!(diff.modified_desc.is_empty());
        assert!(diff.modified_schema.is_empty());
    }

    #[test]
    fn diff_detects_removed_tool() {
        let locked = compute_tool_hashes(&[tool("a", "desc"), tool("b", "desc")]);
        let current = vec![tool("a", "desc")];
        let diff = diff_tool_snapshots(&locked, &current);
        assert_eq!(diff.removed, vec!["b"]);
        assert!(diff.added.is_empty());
    }

    #[test]
    fn diff_detects_modified_description() {
        let locked = compute_tool_hashes(&[tool("a", "original")]);
        let current = vec![tool("a", "poisoned")];
        let diff = diff_tool_snapshots(&locked, &current);
        assert_eq!(diff.modified_desc, vec!["a"]);
        assert!(diff.modified_schema.is_empty());
    }

    #[test]
    fn diff_detects_modified_schema() {
        let locked = compute_tool_hashes(&[tool("a", "desc")]);
        let mut changed = tool("a", "desc");
        changed.input_schema = json!({"type": "object", "properties": {"q": {"type": "string"}}});
        let diff = diff_tool_snapshots(&locked, &[changed]);
        assert!(diff.modified_desc.is_empty());
        assert_eq!(diff.modified_schema, vec!["a"]);
    }

    #[test]
    fn diff_empty_when_no_changes() {
        let locked = compute_tool_hashes(&[tool("a", "desc")]);
        let current = vec![tool("a", "desc")];
        let diff = diff_tool_snapshots(&locked, &current);
        assert!(diff.is_empty());
    }
}
```

- [ ] **Step 4: 运行测试验证通过**

Run: `cargo test -p llm-harness-runtime-mcp version_lock::tests -- --nocapture`
Expected: 6 tests PASS

- [ ] **Step 5: Commit**

```bash
git add crates/llm-harness-runtime-mcp/src/version_lock.rs crates/llm-harness-runtime-mcp/src/lib.rs crates/llm-harness-runtime-mcp/Cargo.toml
git commit -m "feat(mcp): add version lock file structures and tool diff

McpLockFile / ServerLockEntry / ToolSnapshotEntry data structures,
sha256 hashing for description and input_schema, diff_tool_snapshots
detecting added/removed/modified_desc/modified_schema changes."
```

---

### Task 2: 锁文件读写

**Files:**
- Modify: `crates/llm-harness-runtime-mcp/src/version_lock.rs`
- Modify: `crates/llm-harness-runtime-mcp/Cargo.toml`（新增 tempfile dev-dependency）
- Test: 内联 `#[cfg(test)] mod tests` 追加

**Interfaces:**
- Consumes: `McpLockFile`, `ServerLockEntry`, `compute_tool_hashes` from Task 1
- Produces: `McpLockFile::load_from_disk(&Path) -> Result<McpLockFile>`, `McpLockFile::save_to_disk(&Path) -> Result<()>`, `McpLockFile::get_entry(&str) -> Option<&ServerLockEntry>`, `McpLockFile::upsert_entry(String, ServerLockEntry)`

- [ ] **Step 1: 添加 tempfile dev-dependency**

检查 `crates/llm-harness-runtime-mcp/Cargo.toml` 的 `[dev-dependencies]` 是否有 `tempfile`。如果没有，添加：

```toml
tempfile = "3"
```

- [ ] **Step 2: 写失败测试——锁文件读写 round-trip**

在 `version_lock.rs` 的 `mod tests` 追加：

```rust
    #[tokio::test]
    async fn lock_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".mcp-lock.json");

        let mut lock = McpLockFile::default();
        let entry = ServerLockEntry {
            server_info: ServerInfoSnapshot {
                name: "github-mcp".to_owned(),
                version: "1.2.3".to_owned(),
            },
            protocol_version: "2025-11-25".to_owned(),
            tools_snapshot: compute_tool_hashes(&[tool("search", "desc")]),
        };
        lock.upsert_entry("github".to_owned(), entry.clone());

        lock.save_to_disk(&path).await.unwrap();

        let loaded = McpLockFile::load_from_disk(&path).await.unwrap();
        assert_eq!(loaded.version, 1);
        let got = loaded.get_entry("github").unwrap();
        assert_eq!(got.server_info.version, "1.2.3");
        assert_eq!(got.tools_snapshot.len(), 1);
        assert_eq!(got.tools_snapshot[0].name, "search");
    }

    #[tokio::test]
    async fn load_nonexistent_returns_default() {
        let path = std::path::Path::new("/nonexistent/.mcp-lock.json");
        let lock = McpLockFile::load_from_disk(&path).await.unwrap();
        assert!(lock.servers.is_empty());
    }
```

- [ ] **Step 3: 运行测试验证失败**

Run: `cargo test -p llm-harness-runtime-mcp version_lock::tests::lock_file_round_trip`
Expected: FAIL（方法不存在）

- [ ] **Step 4: 实现 load_from_disk / save_to_disk / get_entry / upsert_entry**

在 `version_lock.rs` 添加 `impl McpLockFile` 块（在结构体定义之后）：

```rust
impl McpLockFile {
    pub async fn load_from_disk(path: &std::path::Path) -> anyhow::Result<Self> {
        match tokio::fs::read_to_string(path).await {
            Ok(text) => {
                let lock: Self = serde_json::from_str(&text)
                    .map_err(|e| anyhow::anyhow!("invalid lock file '{}': {e}", path.display()))?;
                Ok(lock)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(anyhow::anyhow!("failed to read lock file '{}': {e}", path.display())),
        }
    }

    pub async fn save_to_disk(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        let text = serde_json::to_string_pretty(self)?;
        tokio::fs::write(path, text).await?;
        Ok(())
    }

    pub fn get_entry(&self, server_name: &str) -> Option<&ServerLockEntry> {
        self.servers.get(server_name)
    }

    pub fn upsert_entry(&mut self, server_name: String, entry: ServerLockEntry) {
        self.servers.insert(server_name, entry);
    }
}
```

需要确保 `anyhow` 在 Cargo.toml 依赖中。检查后如果没有，添加 `anyhow.workspace = true`。

- [ ] **Step 5: 运行测试验证通过**

Run: `cargo test -p llm-harness-runtime-mcp version_lock::tests -- --nocapture`
Expected: 8 tests PASS

- [ ] **Step 6: Commit**

```bash
git add crates/llm-harness-runtime-mcp/src/version_lock.rs crates/llm-harness-runtime-mcp/Cargo.toml
git commit -m "feat(mcp): add lock file load/save and entry accessors"
```

---

### Task 3: VersionPolicy 配置类型

**Files:**
- Modify: `crates/llm-harness-runtime-mcp/src/config.rs`
- Test: 内联 `#[cfg(test)] mod tests` 追加

**Interfaces:**
- Produces: `VersionPolicy`（含 `on_mismatch: MismatchPolicy`, `on_tool_change: ToolChangePolicy`, `lock_file: Option<PathBuf>`）、`MismatchPolicy`（enum: `FailClosed`, `AllowWithWarning`, `AllowAndUpdate`）、`ToolChangePolicy`（enum: `FailClosed`, `AllowAdded`, `AllowAll`）

- [ ] **Step 1: 写失败测试——VersionPolicy 序列化与默认值**

在 `config.rs` 的 `#[cfg(test)]` 模块中追加：

```rust
    #[test]
    fn version_policy_defaults_to_fail_closed() {
        let policy = VersionPolicy::default();
        assert_eq!(policy.on_mismatch, MismatchPolicy::FailClosed);
        assert_eq!(policy.on_tool_change, ToolChangePolicy::FailClosed);
        assert!(policy.lock_file.is_none());
    }

    #[test]
    fn version_policy_deserializes_from_json() {
        let json = r#"{
            "lock_file": ".mcp-lock.json",
            "on_mismatch": "allow_with_warning",
            "on_tool_change": "allow_added"
        }"#;
        let policy: VersionPolicy = serde_json::from_str(json).unwrap();
        assert_eq!(policy.on_mismatch, MismatchPolicy::AllowWithWarning);
        assert_eq!(policy.on_tool_change, ToolChangePolicy::AllowAdded);
        assert_eq!(policy.lock_file.as_deref(), Some(std::path::Path::new(".mcp-lock.json")));
    }
```

- [ ] **Step 2: 运行测试验证失败**

Run: `cargo test -p llm-harness-runtime-mcp config::tests::version_policy`
Expected: FAIL（类型不存在）

- [ ] **Step 3: 实现 VersionPolicy 类型**

在 `config.rs` 中（`McpServerConfig` enum 之前）添加：

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionPolicy {
    #[serde(default)]
    pub lock_file: Option<PathBuf>,
    #[serde(default)]
    pub on_mismatch: MismatchPolicy,
    #[serde(default)]
    pub on_tool_change: ToolChangePolicy,
}

impl Default for VersionPolicy {
    fn default() -> Self {
        Self {
            lock_file: None,
            on_mismatch: MismatchPolicy::FailClosed,
            on_tool_change: ToolChangePolicy::FailClosed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MismatchPolicy {
    #[serde(rename = "fail_closed")]
    FailClosed,
    #[serde(rename = "allow_with_warning")]
    AllowWithWarning,
    #[serde(rename = "allow_and_update")]
    AllowAndUpdate,
}

impl Default for MismatchPolicy {
    fn default() -> Self {
        Self::FailClosed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolChangePolicy {
    #[serde(rename = "fail_closed")]
    FailClosed,
    #[serde(rename = "allow_added")]
    AllowAdded,
    #[serde(rename = "allow_all")]
    AllowAll,
}

impl Default for ToolChangePolicy {
    fn default() -> Self {
        Self::FailClosed
    }
}
```

- [ ] **Step 4: 将 VersionPolicy 添加到 McpServerConfig**

修改 `McpServerConfig` 的两个 variant，各添加 `version_policy` 字段：

```rust
pub enum McpServerConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
        #[serde(default)]
        timeout: Option<u64>,
        #[serde(default)]
        version_policy: VersionPolicy,
        #[serde(default)]
        credentials: Option<crate::credential::CredentialDeclarations>,
    },
    HttpOrSse {
        url: String,
        #[serde(default, rename = "type")]
        transport_type: Option<String>,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        timeout: Option<u64>,
        #[serde(default)]
        version_policy: VersionPolicy,
        #[serde(default)]
        credentials: Option<crate::credential::CredentialDeclarations>,
    },
}
```

注意：`credentials` 字段在 Task 5 中定义 `CredentialDeclarations` 类型后才能编译。为避免前向引用，先只添加 `version_policy`，`credentials` 在 Task 5 中添加。当前 Step 只添加 `version_policy`。

更新所有 match 方法（`timeout_ms`, `validate`, `is_stdio`, `is_sse`, `is_http`）使用 `..` 通配。新增 `version_policy()` 访问器：

```rust
    pub fn version_policy(&self) -> &VersionPolicy {
        match self {
            Self::Stdio { version_policy, .. } | Self::HttpOrSse { version_policy, .. } => version_policy,
        }
    }
```

- [ ] **Step 5: 运行测试验证通过**

Run: `cargo test -p llm-harness-runtime-mcp config::tests -- --nocapture`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/llm-harness-runtime-mcp/src/config.rs
git commit -m "feat(mcp): add VersionPolicy config with fail-closed defaults

VersionPolicy { lock_file, on_mismatch, on_tool_change } with
MismatchPolicy and ToolChangePolicy enums. Defaults to fail_closed
for both mismatch and tool change."
```

---

### Task 4: 凭证引用解析——CredentialProvider trait 与 EnvCredentialProvider

**Files:**
- Create: `crates/llm-harness-runtime-mcp/src/credential.rs`
- Modify: `crates/llm-harness-runtime-mcp/src/lib.rs`（新增 `pub mod credential;`）
- Test: 内联 `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: `CredentialProvider` trait、`EnvCredentialProvider`、`resolve_credential_value(&str, &dyn CredentialProvider) -> Option<String>`（async 纯函数）、`resolve_map(&HashMap<String,String>, &dyn CredentialProvider) -> Result<HashMap<String,String>, String>`（async，返回第一个未解析的引用）

- [ ] **Step 1: 在 lib.rs 添加 module 声明**

在 `crates/llm-harness-runtime-mcp/src/lib.rs` 中 `pub mod version_lock;` 之后添加：

```rust
pub mod credential;
```

- [ ] **Step 2: 写 credential.rs 完整文件（含测试）**

```rust
use std::collections::HashMap;
use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;

/// Mechanism-layer trait: resolve a credential reference to its value.
/// Concrete backends (env, file, vault) are provided by the consumer.
pub trait CredentialProvider: Send + Sync {
    /// Resolve a credential reference. Returns None if not found.
    fn resolve(&self, reference: &str) -> BoxFuture<'_, Option<String>>;
}

/// Default provider: resolves `${ENV:VAR}` and `${ENV:VAR:-default}` from
/// process environment. Does not handle `vault://` references.
pub struct EnvCredentialProvider;

impl CredentialProvider for EnvCredentialProvider {
    fn resolve(&self, reference: &str) -> BoxFuture<'_, Option<String>> {
        let result = if let Some(rest) = reference.strip_prefix("${ENV:") {
            if let Some(close) = rest.find('}') {
                let inner = &rest[..close];
                if let Some((var, default)) = inner.split_once(":-") {
                    Some(std::env::var(var).unwrap_or_else(|_| default.to_owned()))
                } else {
                    std::env::var(inner).ok()
                }
            } else {
                None
            }
        } else {
            None
        };
        async move { result }.boxed()
    }
}

/// Parses a credential value string. Returns:
/// - `Some(value)` for pure text (no reference syntax) — value is the text itself
/// - `Some(value)` for `${ENV:...}` resolved via the provider
/// - `Some(value)` for `vault://...` resolved via the provider
/// - `None` if a reference cannot be resolved
pub async fn resolve_credential_value(
    raw: &str,
    provider: &dyn CredentialProvider,
) -> Option<String> {
    if !raw.contains("${ENV:") && !raw.starts_with("vault://") {
        return Some(raw.to_owned());
    }
    provider.resolve(raw).await
}

/// Resolves all values in an env/headers map, returning a new map with
/// concrete values. Returns Err with the first unresolved reference.
pub async fn resolve_map(
    input: &HashMap<String, String>,
    provider: &dyn CredentialProvider,
) -> Result<HashMap<String, String>, String> {
    let mut output = HashMap::with_capacity(input.len());
    for (key, raw) in input {
        match resolve_credential_value(raw, provider).await {
            Some(value) => {
                output.insert(key.clone(), value);
            }
            None => return Err(raw.clone()),
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pure_text_passthrough() {
        let provider = EnvCredentialProvider;
        let result = resolve_credential_value("application/json", &provider).await;
        assert_eq!(result, Some("application/json".to_owned()));
    }

    #[tokio::test]
    async fn env_var_resolved() {
        std::env::set_var("TEST_MCP_CRED_TOKEN", "secret123");
        let provider = EnvCredentialProvider;
        let result = resolve_credential_value("${ENV:TEST_MCP_CRED_TOKEN}", &provider).await;
        assert_eq!(result, Some("secret123".to_owned()));
        std::env::remove_var("TEST_MCP_CRED_TOKEN");
    }

    #[tokio::test]
    async fn env_var_with_default() {
        std::env::remove_var("TEST_MCP_CRED_MISSING");
        let provider = EnvCredentialProvider;
        let result = resolve_credential_value("${ENV:TEST_MCP_CRED_MISSING:-fallback}", &provider).await;
        assert_eq!(result, Some("fallback".to_owned()));
    }

    #[tokio::test]
    async fn env_var_missing_no_default_returns_none() {
        std::env::remove_var("TEST_MCP_CRED_MISSING2");
        let provider = EnvCredentialProvider;
        let result = resolve_credential_value("${ENV:TEST_MCP_CRED_MISSING2}", &provider).await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn vault_reference_without_provider_returns_none() {
        let provider = EnvCredentialProvider;
        let result = resolve_credential_value("vault://prod/github/token", &provider).await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn resolve_map_returns_error_on_first_unresolved() {
        std::env::remove_var("TEST_MCP_CRED_MISSING3");
        let provider = EnvCredentialProvider;
        let mut input = HashMap::new();
        input.insert("GOOD".to_owned(), "plain".to_owned());
        input.insert("BAD".to_owned(), "${ENV:TEST_MCP_CRED_MISSING3}".to_owned());
        let result = resolve_map(&input, &provider).await;
        assert_eq!(result, Err("${ENV:TEST_MCP_CRED_MISSING3}".to_owned()));
    }
}
```

- [ ] **Step 3: 运行测试验证通过**

Run: `cargo test -p llm-harness-runtime-mcp credential::tests -- --nocapture`
Expected: 6 tests PASS

- [ ] **Step 4: Commit**

```bash
git add crates/llm-harness-runtime-mcp/src/credential.rs crates/llm-harness-runtime-mcp/src/lib.rs
git commit -m "feat(mcp): add CredentialProvider trait and EnvCredentialProvider

CredentialProvider trait for resolving credential references,
EnvCredentialProvider default impl for \${ENV:VAR} and \${ENV:VAR:-default},
resolve_credential_value / resolve_map pure functions for env/headers maps."
```

---

### Task 5: 凭证作用域声明——ScopedCredential 配置类型

**Files:**
- Modify: `crates/llm-harness-runtime-mcp/src/credential.rs`
- Modify: `crates/llm-harness-runtime-mcp/src/config.rs`（McpServerConfig 新增 `credentials` 字段 + 互斥校验）
- Test: 内联测试

**Interfaces:**
- Produces: `ScopedCredential`（`{ value: String, allowed_tools: Option<Vec<String>> }`）、`CredentialDeclarations`（`{ scoped_env: HashMap<String, ScopedCredential> }`）、`McpServerConfig.credentials()` 访问器

- [ ] **Step 1: 写失败测试——ScopedCredential 反序列化与互斥校验**

在 `credential.rs` 的 `mod tests` 追加：

```rust
    #[test]
    fn scoped_credential_deserializes() {
        let json = r#"{
            "value": "${ENV:GITHUB_TOKEN}",
            "allowed_tools": ["search_repos", "get_file_contents"]
        }"#;
        let cred: ScopedCredential = serde_json::from_str(json).unwrap();
        assert_eq!(cred.value, "${ENV:GITHUB_TOKEN}");
        assert_eq!(cred.allowed_tools.as_ref().unwrap(), &vec!["search_repos".to_owned(), "get_file_contents".to_owned()]);
    }

    #[test]
    fn scoped_credential_without_allowed_tools() {
        let json = r#"{"value": "plain_text"}"#;
        let cred: ScopedCredential = serde_json::from_str(json).unwrap();
        assert!(cred.allowed_tools.is_none());
    }
```

在 `config.rs` 的 `mod tests` 追加：

```rust
    #[test]
    fn credentials_and_env_mutual_exclusion_rejected() {
        let json = r#"{
            "mcpServers": {
                "github": {
                    "command": "github-server",
                    "env": {"TOKEN": "val1"},
                    "credentials": {
                        "scoped_env": {
                            "TOKEN": {"value": "val2"}
                        }
                    }
                }
            }
        }"#;
        let result: Result<McpConfigFile, _> = serde_json::from_str(json);
        if let Ok(config) = result {
            assert!(config.validate().is_err(), "env and scoped_env with same key should be rejected");
        }
    }
```

- [ ] **Step 2: 运行测试验证失败**

Run: `cargo test -p llm-harness-runtime-mcp credential::tests::scoped_credential config::tests::credentials_and_env`
Expected: FAIL

- [ ] **Step 3: 实现 ScopedCredential 和 CredentialDeclarations**

在 `credential.rs` 添加（在 `CredentialProvider` trait 之前），需要加 `use serde::{Deserialize, Serialize};`：

```rust
use serde::{Deserialize, Serialize};

/// A credential declaration with optional tool-scope restriction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedCredential {
    pub value: String,
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
}

/// Credential declarations for a single MCP server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialDeclarations {
    #[serde(default)]
    pub scoped_env: HashMap<String, ScopedCredential>,
}
```

- [ ] **Step 4: 将 credentials 字段添加到 McpServerConfig**

在 `config.rs` 的 `McpServerConfig` 两个 variant 各添加（在 `version_policy` 之后）：

```rust
        #[serde(default)]
        credentials: Option<crate::credential::CredentialDeclarations>,
```

新增 `credentials()` 访问器：

```rust
    pub fn credentials(&self) -> Option<&crate::credential::CredentialDeclarations> {
        match self {
            Self::Stdio { credentials, .. } | Self::HttpOrSse { credentials, .. } => credentials.as_ref(),
        }
    }
```

更新 `validate()` 添加互斥检查。在 Stdio 分支和 HttpOrSse 分别添加调用：

```rust
fn check_env_credential_mutex(
    plain: &HashMap<String, String>,
    credentials: &Option<crate::credential::CredentialDeclarations>,
) -> Result<()> {
    if let Some(creds) = credentials {
        for key in creds.scoped_env.keys() {
            if plain.contains_key(key) {
                bail!(
                    "credential key '{key}' must not appear in both env/headers and credentials.scoped_env"
                );
            }
        }
    }
    Ok(())
}
```

在 `validate()` 的 Stdio 分支中（`command`/`cwd` 检查之后）添加 `check_env_credential_mutex(env, credentials)?;`
在 HttpOrSse 分支中（`transport_type` 检查之后）添加 `check_env_credential_mutex(headers, credentials)?;`

- [ ] **Step 5: 运行测试验证通过**

Run: `cargo test -p llm-harness-runtime-mcp credential::tests config::tests -- --nocapture`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/llm-harness-runtime-mcp/src/credential.rs crates/llm-harness-runtime-mcp/src/config.rs
git commit -m "feat(mcp): add ScopedCredential declarations and env/credentials mutex

ScopedCredential { value, allowed_tools } for tool-scoped credentials,
CredentialDeclarations container, McpServerConfig.credentials field,
validate() rejects same key in both env/headers and scoped_env."
```

---

### Task 6: 错误类型扩展

**Files:**
- Modify: `crates/llm-harness-runtime-mcp/src/manager.rs`
- Test: 无独立测试（错误类型被后续 Task 使用时验证）

**Interfaces:**
- Produces: `McpManagerError::VersionMismatch`, `McpManagerError::CredentialUnresolved`, `McpManagerError::ToolDefinitionChanged`

- [ ] **Step 1: 添加错误变体**

在 `manager.rs` 的 `McpManagerError` enum 中（`Cache` 变体之后）添加：

```rust
    #[error("MCP server '{server}' version mismatch: expected {expected}, got {actual}")]
    VersionMismatch {
        server: String,
        expected: String,
        actual: String,
    },

    #[error("MCP server '{server}' credential reference '{reference}' could not be resolved")]
    CredentialUnresolved {
        server: String,
        reference: String,
    },

    #[error("MCP server '{server}' tool definition changed: {summary}")]
    ToolDefinitionChanged {
        server: String,
        summary: String,
    },
```

- [ ] **Step 2: 验证编译通过**

Run: `cargo build -p llm-harness-runtime-mcp`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add crates/llm-harness-runtime-mcp/src/manager.rs
git commit -m "feat(mcp): add VersionMismatch, CredentialUnresolved, ToolDefinitionChanged errors"
```

---

### Task 7: 版本锁定集成到连接路径

**Files:**
- Modify: `crates/llm-harness-runtime-mcp/src/manager.rs`
- Test: 内联测试

**Interfaces:**
- Consumes: `McpLockFile`, `compute_tool_hashes`, `diff_tool_snapshots` from Task 1-2; `VersionPolicy`, `MismatchPolicy`, `ToolChangePolicy` from Task 3; `McpImplementation` from types
- Produces: `ManagerInner` 新增 `lock_file: RwLock<McpLockFile>` 和 `lock_file_path: Option<PathBuf>` 字段; `verify_and_lock()` 自由函数; `McpManager::with_connector_and_lock()` 构造器

**Context from codebase:**
- `ManagerInner` 在 manager.rs:199-211
- `with_connector` 在 manager.rs:226-242
- `run_connection_attempt` 在 manager.rs:592-640，`Ok((session, tools))` 分支在 623-633
- `discover_and_connect` 在 manager.rs:244-279
- `FixtureTransport` 在 manager.rs:825-905，有 `new(tool_name)` 构造器，返回 serverInfo version="1"，一个工具 description="fixture"
- `stdio_config` 在 manager.rs:907-915

- [ ] **Step 1: 添加新字段到 ManagerInner**

在 `manager.rs` 的 `ManagerInner` struct 中（`shutting_down` 之后）添加：

```rust
    lock_file: RwLock<crate::version_lock::McpLockFile>,
    lock_file_path: Option<PathBuf>,
```

在文件顶部添加导入：

```rust
use crate::version_lock::{
    McpLockFile, ServerLockEntry, ServerInfoSnapshot, compute_tool_hashes, diff_tool_snapshots,
};
use crate::config::{VersionPolicy, MismatchPolicy, ToolChangePolicy};
```

- [ ] **Step 2: 添加 with_connector_and_lock 构造器**

修改 `with_connector` 方法初始化新字段，并添加 `with_connector_and_lock`：

```rust
    pub(crate) fn with_connector(cache_path: Option<PathBuf>, connector: SessionConnector) -> Self {
        Self {
            inner: Arc::new(ManagerInner {
                connections: RwLock::new(HashMap::new()),
                pending_connections: RwLock::new(HashMap::new()),
                server_configs: RwLock::new(HashMap::new()),
                tools: RwLock::new(Vec::new()),
                failures: RwLock::new(HashMap::new()),
                reconnect_history: RwLock::new(HashMap::new()),
                tool_cache: McpToolCache::new(cache_path),
                connection_tasks: Mutex::new(HashMap::new()),
                reconnection_tasks: Mutex::new(HashMap::new()),
                connector,
                shutting_down: AtomicBool::new(false),
                lock_file: RwLock::new(McpLockFile::default()),
                lock_file_path: None,
            }),
        }
    }

    pub(crate) fn with_connector_and_lock(
        cache_path: Option<PathBuf>,
        connector: SessionConnector,
        lock_file_path: PathBuf,
    ) -> Self {
        let mut manager = Self::with_connector(cache_path, connector);
        // Safe to mutate through Arc::get_mut since this is the only reference
        if let Some(inner) = Arc::get_mut(&mut manager.inner) {
            inner.lock_file_path = Some(lock_file_path);
        }
        manager
    }
```

- [ ] **Step 3: 实现 verify_and_lock 自由函数**

在 `manager.rs` 中（`run_connection_attempt` 之前）添加：

```rust
async fn verify_and_lock(
    inner: &Arc<ManagerInner>,
    server_name: &str,
    session: &Arc<McpSession>,
    tools: &[McpToolDefinition],
    policy: &VersionPolicy,
) -> ManagerResult<()> {
    let server_info = session.server_info();
    let protocol_version = session.protocol_version();
    let current_snapshot = compute_tool_hashes(tools);

    let mut lock = inner.lock_file.write();

    match lock.get_entry(server_name) {
        None => {
            let entry = ServerLockEntry {
                server_info: ServerInfoSnapshot {
                    name: server_info.name.clone(),
                    version: server_info.version.clone(),
                },
                protocol_version: protocol_version.clone(),
                tools_snapshot: current_snapshot,
            };
            lock.upsert_entry(server_name.to_owned(), entry);
            drop(lock);
            persist_lock_file(inner).await;
            Ok(())
        }
        Some(entry) => {
            if entry.server_info.version != server_info.version
                || entry.protocol_version != protocol_version
            {
                match policy.on_mismatch {
                    MismatchPolicy::FailClosed => {
                        return Err(McpManagerError::VersionMismatch {
                            server: server_name.to_owned(),
                            expected: entry.server_info.version.clone(),
                            actual: server_info.version.clone(),
                        });
                    }
                    MismatchPolicy::AllowWithWarning => {
                        tracing::warn!(
                            server = server_name,
                            expected = %entry.server_info.version,
                            actual = %server_info.version,
                            "MCP server version mismatch (allowed with warning)"
                        );
                    }
                    MismatchPolicy::AllowAndUpdate => {
                        tracing::info!(
                            server = server_name,
                            "updating lock file for version change"
                        );
                    }
                }
            }

            let diff = diff_tool_snapshots(&entry.tools_snapshot, tools);
            if !diff.is_empty() {
                let should_accept = match policy.on_tool_change {
                    ToolChangePolicy::FailClosed => false,
                    ToolChangePolicy::AllowAdded => {
                        diff.removed.is_empty()
                            && diff.modified_desc.is_empty()
                            && diff.modified_schema.is_empty()
                    }
                    ToolChangePolicy::AllowAll => true,
                };

                if !should_accept {
                    return Err(McpManagerError::ToolDefinitionChanged {
                        server: server_name.to_owned(),
                        summary: diff.summary(),
                    });
                }

                tracing::warn!(
                    server = server_name,
                    summary = %diff.summary(),
                    "MCP tool definition changed (accepted by policy)"
                );
            }

            let updated_entry = ServerLockEntry {
                server_info: ServerInfoSnapshot {
                    name: server_info.name.clone(),
                    version: server_info.version.clone(),
                },
                protocol_version: protocol_version.clone(),
                tools_snapshot: current_snapshot,
            };
            lock.upsert_entry(server_name.to_owned(), updated_entry);
            drop(lock);
            persist_lock_file(inner).await;
            Ok(())
        }
    }
}

async fn persist_lock_file(inner: &Arc<ManagerInner>) {
    if let Some(path) = &inner.lock_file_path {
        let lock = inner.lock_file.read().clone();
        if let Err(error) = lock.save_to_disk(path).await {
            tracing::warn!(%error, "failed to persist MCP lock file");
        }
    }
}
```

- [ ] **Step 4: 在 run_connection_attempt 中调用 verify_and_lock**

修改 `run_connection_attempt` 的 `Ok((session, tools))` 分支（当前在 manager.rs:623-633），在 `wire_session` 之前插入版本锁定：

```rust
        Ok((session, tools)) => {
            let policy = config.version_policy().clone();
            if let Err(error) = verify_and_lock(&inner, &name, &session, &tools, &policy).await {
                let error_msg = error.to_string();
                inner.failures.write().insert(name.clone(), error_msg.clone());
                if let Err(disconnect_error) = session.disconnect().await {
                    tracing::debug!(server = name, %disconnect_error, "failed to disconnect rejected MCP server");
                }
                let _ = sender.send(ConnectionAttempt::Failed(Arc::new(error_msg)));
                inner.pending_connections.write().remove(&name);
                return;
            }
            wire_session(&inner, &name, &session);
            inner
                .connections
                .write()
                .insert(name.clone(), Arc::clone(&session));
            inner.failures.write().remove(&name);
            replace_server_tools(&inner, &name, tools.clone(), false);
            inner.tool_cache.set(&name, tools).await;
            let _ = sender.send(ConnectionAttempt::Ready(session));
        }
```

- [ ] **Step 5: 在 discover_and_connect 中加载锁文件**

在 `McpManager::discover_and_connect` 方法中，`tool_cache.load_from_disk()` 之后添加：

```rust
        if let Some(lock_path) = &self.inner.lock_file_path {
            match McpLockFile::load_from_disk(lock_path).await {
                Ok(lock) => *self.inner.lock_file.write() = lock,
                Err(error) => tracing::warn!(%error, "failed to load MCP lock file"),
            }
        }
```

- [ ] **Step 6: 写测试**

在 `manager.rs` 的 `mod tests` 追加。需要先在 `FixtureTransport` 中添加 `new_with_desc` 和 `new_with_version` 构造器：

```rust
    fn new_with_desc(tool_name: &str, desc: &str) -> Self {
        Self {
            connected: AtomicBool::new(true),
            responses: Mutex::new(
                [
                    json!({
                        "protocolVersion": crate::session::PREFERRED_PROTOCOL_VERSION,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "fixture", "version": "1"}
                    }),
                    json!({
                        "tools": [{
                            "name": tool_name,
                            "description": desc,
                            "inputSchema": {"type": "object"}
                        }]
                    }),
                ]
                .into(),
            ),
            request_handler: Mutex::new(None),
            drop_tracker: None,
        }
    }

    fn new_with_version(tool_name: &str, version: &str) -> Self {
        Self {
            connected: AtomicBool::new(true),
            responses: Mutex::new(
                [
                    json!({
                        "protocolVersion": crate::session::PREFERRED_PROTOCOL_VERSION,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "fixture", "version": version}
                    }),
                    json!({
                        "tools": [{
                            "name": tool_name,
                            "description": "fixture",
                            "inputSchema": {"type": "object"}
                        }]
                    }),
                ]
                .into(),
            ),
            request_handler: Mutex::new(None),
            drop_tracker: None,
        }
    }
```

测试用例：

```rust
    #[tokio::test]
    async fn first_connection_writes_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(".mcp-lock.json");

        let connector: SessionConnector = Arc::new(|_config| {
            Box::pin(async move {
                let transport: Arc<dyn McpTransport> = Arc::new(FixtureTransport::new("tool_a"));
                Ok(Arc::new(McpSession::connect(transport).await?))
            })
        });
        let manager = McpManager::with_connector_and_lock(None, connector, lock_path.clone());

        manager
            .discover_and_connect(HashMap::from([
                ("srv".to_owned(), stdio_config("srv")),
            ]))
            .await
            .unwrap();

        let lock = McpLockFile::load_from_disk(&lock_path).await.unwrap();
        let entry = lock.get_entry("srv").unwrap();
        assert_eq!(entry.server_info.name, "fixture");
        assert_eq!(entry.server_info.version, "1");
        assert_eq!(entry.tools_snapshot.len(), 1);
        assert_eq!(entry.tools_snapshot[0].name, "tool_a");
    }

    #[tokio::test]
    async fn version_mismatch_fail_closed_rejects_connection() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(".mcp-lock.json");

        let mut lock = McpLockFile::default();
        lock.upsert_entry("srv".to_owned(), ServerLockEntry {
            server_info: ServerInfoSnapshot { name: "fixture".to_owned(), version: "2.0.0".to_owned() },
            protocol_version: crate::session::PREFERRED_PROTOCOL_VERSION.to_owned(),
            tools_snapshot: compute_tool_hashes(&[McpToolDefinition {
                name: "tool_a".to_owned(),
                title: None,
                description: "fixture".to_owned(),
                input_schema: json!({"type": "object"}),
                output_schema: None,
                annotations: None,
                icons: Vec::new(),
                metadata: None,
            }]),
        });
        lock.save_to_disk(&lock_path).await.unwrap();

        let connector: SessionConnector = Arc::new(|_config| {
            Box::pin(async move {
                let transport: Arc<dyn McpTransport> = Arc::new(FixtureTransport::new("tool_a"));
                Ok(Arc::new(McpSession::connect(transport).await?))
            })
        });
        let manager = McpManager::with_connector_and_lock(None, connector, lock_path);

        manager
            .discover_and_connect(HashMap::from([
                ("srv".to_owned(), stdio_config("srv")),
            ]))
            .await
            .unwrap();

        assert_eq!(
            manager.get_connection_status("srv"),
            ConnectionStatus::Disconnected
        );
        assert!(manager.errors().contains_key("srv"));
        assert!(manager.errors()["srv"].contains("version mismatch"));
    }

    #[tokio::test]
    async fn tool_change_fail_closed_rejects_connection() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(".mcp-lock.json");

        let original_tool = McpToolDefinition {
            name: "tool_a".to_owned(),
            title: None,
            description: "original".to_owned(),
            input_schema: json!({"type": "object"}),
            output_schema: None,
            annotations: None,
            icons: Vec::new(),
            metadata: None,
        };
        let mut lock = McpLockFile::default();
        lock.upsert_entry("srv".to_owned(), ServerLockEntry {
            server_info: ServerInfoSnapshot { name: "fixture".to_owned(), version: "1".to_owned() },
            protocol_version: crate::session::PREFERRED_PROTOCOL_VERSION.to_owned(),
            tools_snapshot: compute_tool_hashes(&[original_tool]),
        });
        lock.save_to_disk(&lock_path).await.unwrap();

        let connector: SessionConnector = Arc::new(|_config| {
            Box::pin(async move {
                let transport: Arc<dyn McpTransport> = Arc::new(
                    FixtureTransport::new_with_desc("tool_a", "poisoned"),
                );
                Ok(Arc::new(McpSession::connect(transport).await?))
            })
        });
        let manager = McpManager::with_connector_and_lock(None, connector, lock_path);

        manager
            .discover_and_connect(HashMap::from([
                ("srv".to_owned(), stdio_config("srv")),
            ]))
            .await
            .unwrap();

        assert_eq!(
            manager.get_connection_status("srv"),
            ConnectionStatus::Disconnected
        );
        assert!(manager.errors()["srv"].contains("tool definition changed"));
    }
```

- [ ] **Step 7: 运行测试验证通过**

Run: `cargo test -p llm-harness-runtime-mcp manager::tests -- --nocapture`
Expected: 所有测试 PASS

- [ ] **Step 8: Commit**

```bash
git add crates/llm-harness-runtime-mcp/src/manager.rs
git commit -m "feat(mcp): integrate version lock into connection path

verify_and_lock() checks server_info.version + protocol_version + tool
definition hashes against lock file. First connection writes snapshot;
subsequent connections diff and apply VersionPolicy (fail_closed default)."
```

---

### Task 8: tools/list_changed 回调改造

**Files:**
- Modify: `crates/llm-harness-runtime-mcp/src/manager.rs`
- Test: 内联测试

**Interfaces:**
- Consumes: `verify_and_lock` from Task 7

- [ ] **Step 1: 改造 wire_session 中的 tools/list_changed 回调**

在 `wire_session` 函数中（manager.rs:642-677），修改 `on_notification` 回调。当前代码直接 `replace_server_tools` + `tool_cache.set`。改为先走 `verify_and_lock`：

```rust
    session.on_notification(Arc::new(move |notification| {
        if notification.method != notification_methods::TOOLS_LIST_CHANGED {
            return;
        }
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let name = notification_name.clone();
        tokio::spawn(async move {
            let session = inner.connections.read().get(&name).cloned();
            let Some(session) = session else { return };

            let tools = match session.list_tools().await {
                Ok(tools) => tools,
                Err(error) => {
                    tracing::warn!(server = name, %error, "failed to refresh MCP tools");
                    return;
                }
            };

            let policy = inner
                .server_configs
                .read()
                .get(&name)
                .map(|c| c.version_policy().clone())
                .unwrap_or_default();

            match verify_and_lock(&inner, &name, &session, &tools, &policy).await {
                Ok(()) => {
                    replace_server_tools(&inner, &name, tools.clone(), false);
                    inner.tool_cache.set(&name, tools).await;
                }
                Err(error) => {
                    tracing::warn!(
                        server = name,
                        %error,
                        "rejected tools/list_changed update (policy: fail_closed)"
                    );
                }
            }
        });
    }));
```

- [ ] **Step 2: 运行测试验证通过**

Run: `cargo test -p llm-harness-runtime-mcp manager::tests -- --nocapture`
Expected: 所有测试 PASS

- [ ] **Step 3: Commit**

```bash
git add crates/llm-harness-runtime-mcp/src/manager.rs
git commit -m "feat(mcp): route tools/list_changed through verify_and_lock

tools/list_changed notification no longer silently refreshes tools.
New tools are diffed against the lock file snapshot and subject to
ToolChangePolicy. fail_closed keeps old tools on mismatch."
```

---

### Task 9: 凭证解析集成到连接路径

**Files:**
- Modify: `crates/llm-harness-runtime-mcp/src/manager.rs`
- Test: 内联测试

**Interfaces:**
- Consumes: `CredentialProvider`, `EnvCredentialProvider`, `resolve_map`, `resolve_credential_value` from Task 4; `CredentialDeclarations`, `ScopedCredential` from Task 5
- Produces: `ManagerInner` 新增 `credential_provider: Arc<dyn CredentialProvider>` 字段; `resolve_config_credentials` 和 `resolve_scoped_credentials` 自由函数

- [ ] **Step 1: 添加 credential_provider 到 ManagerInner**

在 `ManagerInner` 中添加：

```rust
    credential_provider: Arc<dyn crate::credential::CredentialProvider>,
```

修改 `with_connector` 初始化新字段：

```rust
                credential_provider: Arc::new(crate::credential::EnvCredentialProvider),
```

在 `with_connector_and_lock` 中不需要特殊处理（使用默认 `EnvCredentialProvider`）。如果用户需要自定义 provider，通过新构造器注入。添加：

```rust
    pub fn with_credential_provider(
        cache_path: Option<PathBuf>,
        connector: SessionConnector,
        lock_file_path: Option<PathBuf>,
        provider: Arc<dyn crate::credential::CredentialProvider>,
    ) -> Self {
        let mut manager = Self::with_connector(cache_path, connector);
        if let Some(inner) = Arc::get_mut(&mut manager.inner) {
            if let Some(path) = lock_file_path {
                inner.lock_file_path = Some(path);
            }
            // Can't replace Arc<dyn CredentialProvider> through &mut since
            // other Arc clones may exist. This constructor must be called
            // before any other reference is taken.
            // For safety, we rebuild the inner:
        }
        // Actually, rebuild via a private constructor:
        Self::build_inner(cache_path, connector, lock_file_path, provider)
    }

    fn build_inner(
        cache_path: Option<PathBuf>,
        connector: SessionConnector,
        lock_file_path: Option<PathBuf>,
        credential_provider: Arc<dyn crate::credential::CredentialProvider>,
    ) -> Self {
        Self {
            inner: Arc::new(ManagerInner {
                connections: RwLock::new(HashMap::new()),
                pending_connections: RwLock::new(HashMap::new()),
                server_configs: RwLock::new(HashMap::new()),
                tools: RwLock::new(Vec::new()),
                failures: RwLock::new(HashMap::new()),
                reconnect_history: RwLock::new(HashMap::new()),
                tool_cache: McpToolCache::new(cache_path),
                connection_tasks: Mutex::new(HashMap::new()),
                reconnection_tasks: Mutex::new(HashMap::new()),
                connector,
                shutting_down: AtomicBool::new(false),
                lock_file: RwLock::new(McpLockFile::default()),
                lock_file_path,
                credential_provider,
            }),
        }
    }
```

简化：将 `with_connector` 改为调用 `build_inner`：

```rust
    pub(crate) fn with_connector(cache_path: Option<PathBuf>, connector: SessionConnector) -> Self {
        Self::build_inner(
            cache_path,
            connector,
            None,
            Arc::new(crate::credential::EnvCredentialProvider),
        )
    }

    pub(crate) fn with_connector_and_lock(
        cache_path: Option<PathBuf>,
        connector: SessionConnector,
        lock_file_path: PathBuf,
    ) -> Self {
        Self::build_inner(
            cache_path,
            connector,
            Some(lock_file_path),
            Arc::new(crate::credential::EnvCredentialProvider),
        )
    }
```

- [ ] **Step 2: 实现 resolve_config_credentials 自由函数**

在 `manager.rs` 中（`run_connection_attempt` 之前）添加：

```rust
async fn resolve_config_credentials(
    inner: &Arc<ManagerInner>,
    server_name: &str,
    config: &McpServerConfig,
) -> ManagerResult<McpServerConfig> {
    use crate::credential::{resolve_credential_value, resolve_map, CredentialDeclarations, ScopedCredential};

    let provider = inner.credential_provider.as_ref();
    let mut resolved = config.clone();

    match &mut resolved {
        McpServerConfig::Stdio { env, credentials, .. } => {
            let resolved_env = resolve_map(env, provider)
                .await
                .map_err(|ref_str| McpManagerError::CredentialUnresolved {
                    server: server_name.to_owned(),
                    reference: ref_str,
                })?;
            *env = resolved_env;

            if let Some(creds) = credentials {
                let resolved_creds = resolve_scoped_credentials(creds, provider)
                    .await
                    .map_err(|ref_str| McpManagerError::CredentialUnresolved {
                        server: server_name.to_owned(),
                        reference: ref_str,
                    })?;
                *credentials = Some(resolved_creds);
            }
        }
        McpServerConfig::HttpOrSse { headers, credentials, .. } => {
            let resolved_headers = resolve_map(headers, provider)
                .await
                .map_err(|ref_str| McpManagerError::CredentialUnresolved {
                    server: server_name.to_owned(),
                    reference: ref_str,
                })?;
            *headers = resolved_headers;

            if let Some(creds) = credentials {
                let resolved_creds = resolve_scoped_credentials(creds, provider)
                    .await
                    .map_err(|ref_str| McpManagerError::CredentialUnresolved {
                        server: server_name.to_owned(),
                        reference: ref_str,
                    })?;
                *credentials = Some(resolved_creds);
            }
        }
    }

    Ok(resolved)
}

async fn resolve_scoped_credentials(
    creds: &CredentialDeclarations,
    provider: &dyn crate::credential::CredentialProvider,
) -> Result<CredentialDeclarations, String> {
    let mut resolved = CredentialDeclarations::default();
    for (key, scoped) in &creds.scoped_env {
        let value = resolve_credential_value(&scoped.value, provider)
            .await
            .ok_or_else(|| scoped.value.clone())?;
        resolved.scoped_env.insert(
            key.clone(),
            ScopedCredential {
                value,
                allowed_tools: scoped.allowed_tools.clone(),
            },
        );
    }
    Ok(resolved)
}
```

- [ ] **Step 3: 在 run_connection_attempt 中调用凭证解析**

修改 `run_connection_attempt` 函数。在 `config.validate()` 之后、`let connect = async {` 之前，插入：

```rust
    let config = match resolve_config_credentials(&inner, &name, &config).await {
        Ok(config) => config,
        Err(error) => {
            let error_msg = error.to_string();
            inner.failures.write().insert(name.clone(), error_msg.clone());
            let _ = sender.send(ConnectionAttempt::Failed(Arc::new(error_msg)));
            inner.pending_connections.write().remove(&name);
            return;
        }
    };
```

注意：`config` 参数需要从 `config: McpServerConfig` 改为 `mut config` 或 shadow（用 `let config = ...` 覆盖）。由于 `run_connection_attempt` 当前签名是 `config: McpServerConfig`（by value），可以直接 shadow。

- [ ] **Step 4: 写测试**

在 `manager.rs` 的 `mod tests` 追加：

```rust
    #[tokio::test]
    async fn unresolved_credential_rejects_connection() {
        std::env::remove_var("TEST_MCP_UNRESOLVED_TOKEN");
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(".mcp-lock.json");

        let connector: SessionConnector = Arc::new(|_config| {
            Box::pin(async move {
                let transport: Arc<dyn McpTransport> = Arc::new(FixtureTransport::new("tool_a"));
                Ok(Arc::new(McpSession::connect(transport).await?))
            })
        });
        let manager = McpManager::with_connector_and_lock(None, connector, lock_path);

        let mut config = stdio_config("srv");
        if let McpServerConfig::Stdio { env, .. } = &mut config {
            env.insert("TOKEN".to_owned(), "${ENV:TEST_MCP_UNRESOLVED_TOKEN}".to_owned());
        }

        manager
            .discover_and_connect(HashMap::from([
                ("srv".to_owned(), config),
            ]))
            .await
            .unwrap();

        assert_eq!(
            manager.get_connection_status("srv"),
            ConnectionStatus::Disconnected
        );
        assert!(manager.errors()["srv"].contains("credential"));
    }
```

- [ ] **Step 5: 运行测试验证通过**

Run: `cargo test -p llm-harness-runtime-mcp manager::tests -- --nocapture`
Expected: 所有测试 PASS

- [ ] **Step 6: Commit**

```bash
git add crates/llm-harness-runtime-mcp/src/manager.rs
git commit -m "feat(mcp): integrate credential reference resolution into connection

Env/headers and scoped_env credential references are resolved before
transport creation. Unresolved references reject the connection
(fail-closed). Resolved values exist only in transport memory."
```

---

### Task 10: McpToolAdapter 暴露 authorized_credentials

**Files:**
- Modify: `crates/llm-harness-runtime-mcp/src/lib.rs`
- Modify: `crates/llm-harness-runtime-mcp/src/manager.rs`（ManagerInner 新增 credential_scopes 字段 + authorized_credentials_for 方法 + 注册作用域逻辑）
- Test: 内联测试

**Interfaces:**
- Consumes: `CredentialDeclarations`, `ScopedCredential` from Task 5
- Produces: `McpToolAdapter.authorized_credentials: Vec<String>`（pub 字段）, `McpManager::authorized_credentials_for(server_name, tool_name) -> Vec<String>`

- [ ] **Step 1: 添加 credential_scopes 到 ManagerInner**

在 `manager.rs` 的 `ManagerInner` 中添加：

```rust
    credential_scopes: RwLock<HashMap<String, HashMap<String, Vec<String>>>>,
```

在 `build_inner` 中初始化：

```rust
                credential_scopes: RwLock::new(HashMap::new()),
```

- [ ] **Step 2: 在连接成功后注册凭证作用域**

在 `run_connection_attempt` 的 `Ok((session, tools))` 分支中，`verify_and_lock` 之后、`wire_session` 之前，添加：

```rust
            // Register credential scopes
            if let Some(creds) = config.credentials() {
                let scopes: HashMap<String, Vec<String>> = creds
                    .scoped_env
                    .iter()
                    .filter_map(|(key, scoped)| {
                        scoped.allowed_tools.as_ref().map(|tools| (key.clone(), tools.clone()))
                    })
                    .collect();
                if !scopes.is_empty() {
                    inner.credential_scopes.write().insert(name.clone(), scopes);
                }
            }
```

- [ ] **Step 3: 实现 authorized_credentials_for 方法**

在 `McpManager` impl 中添加：

```rust
    /// Returns the set of credential keys authorized for the given tool
    /// (raw `tool_def.name` before `sanitize_name`) on the given server.
    pub fn authorized_credentials_for(&self, server_name: &str, tool_name: &str) -> Vec<String> {
        let scopes = self.inner.credential_scopes.read();
        let Some(server_scopes) = scopes.get(server_name) else {
            return Vec::new();
        };
        server_scopes
            .iter()
            .filter_map(|(key, allowed_tools)| {
                if allowed_tools.contains(tool_name) {
                    Some(key.clone())
                } else {
                    None
                }
            })
            .collect()
    }
```

- [ ] **Step 4: 修改 McpToolAdapter 和 into_adapters**

在 `lib.rs` 中修改 `McpToolAdapter` struct：

```rust
pub struct McpToolAdapter {
    full_name: String,
    description: String,
    parameters_schema: Value,
    manager: Arc<McpManager>,
    /// Credential keys authorized for this tool (from scoped_env declarations).
    pub authorized_credentials: Vec<String>,
}
```

修改 `into_adapters`：

```rust
    pub async fn into_adapters(&self) -> Result<Vec<McpToolAdapter>> {
        Ok(self
            .manager
            .get_tools()
            .into_iter()
            .map(|tool| {
                let authorized = self
                    .manager
                    .authorized_credentials_for(&tool.server_name, &tool.tool_def.name);
                McpToolAdapter {
                    full_name: tool.full_name,
                    description: tool.tool_def.description,
                    parameters_schema: tool.tool_def.input_schema,
                    manager: Arc::clone(&self.manager),
                    authorized_credentials: authorized,
                }
            })
            .collect())
    }
```

- [ ] **Step 5: 写测试**

在 `manager.rs` 的 `mod tests` 追加：

```rust
    #[tokio::test]
    async fn authorized_credentials_populated_for_scoped_tool() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(".mcp-lock.json");

        let connector: SessionConnector = Arc::new(|_config| {
            Box::pin(async move {
                let transport: Arc<dyn McpTransport> = Arc::new(FixtureTransport::new("search_repos"));
                Ok(Arc::new(McpSession::connect(transport).await?))
            })
        });
        let manager = McpManager::with_connector_and_lock(None, connector, lock_path);

        let mut config = stdio_config("srv");
        let creds = crate::credential::CredentialDeclarations {
            scoped_env: {
                let mut map = HashMap::new();
                map.insert("TOKEN".to_owned(), crate::credential::ScopedCredential {
                    value: "plain".to_owned(),
                    allowed_tools: Some(vec!["search_repos".to_owned()]),
                });
                map
            },
        };
        if let McpServerConfig::Stdio { credentials, .. } = &mut config {
            *credentials = Some(creds);
        }

        manager
            .discover_and_connect(HashMap::from([("srv".to_owned(), config)]))
            .await
            .unwrap();

        let source = McpToolSource::new(Arc::new(manager));
        let adapters = source.into_adapters().await.unwrap();
        assert_eq!(adapters.len(), 1);
        assert_eq!(adapters[0].authorized_credentials, vec!["TOKEN".to_owned()]);
    }
```

- [ ] **Step 6: 运行测试验证通过**

Run: `cargo test -p llm-harness-runtime-mcp -- --nocapture`
Expected: 所有测试 PASS

- [ ] **Step 7: Commit**

```bash
git add crates/llm-harness-runtime-mcp/src/lib.rs crates/llm-harness-runtime-mcp/src/manager.rs
git commit -m "feat(mcp): expose authorized_credentials on McpToolAdapter

ManagerInner.credential_scopes maps server→credential_key→allowed_tools.
into_adapters() populates authorized_credentials by reverse-looking-up
which scoped credentials allow the current tool."
```

---

### Task 11: 策略层——McpCredentialGuardHook

**Files:**
- Create: `crates/llm-harness-strategy/src/credential_guard.rs`
- Modify: `crates/llm-harness-strategy/src/lib.rs`（添加 `pub mod credential_guard;`）
- Modify: `crates/llm-harness-strategy/Cargo.toml`（新增 `tracing` 依赖）
- Test: 内联测试

**Interfaces:**
- Consumes: `BeforeToolCallHook`, `BeforeToolCallCtx`, `BeforeToolCallDecision` from `llm-harness-types`
- Produces: `McpCredentialGuardHook` struct（实现 `BeforeToolCallHook`）

**设计决策**：`BeforeToolCallCtx` 只有 `tool_name`，无法直接获取 `McpToolAdapter.authorized_credentials`。hook 持有一个查询闭包 `credential_lookup: Arc<dyn Fn(&str) -> CredentialLookup + Send + Sync>`，由调用方注入。`CredentialLookup` 是一个枚举：`NotScoped`（该工具未配置作用域，不告警）、`Authorized(Vec<String>)`（该工具在白名单内，不告警）、`Unauthorized`（该工具不在任何已声明凭证的 `allowed_tools` 内，但服务器配置了 `scoped_env`——告警）。

调用方在构造 hook 时，从 `McpManager` 的 `credential_scopes` 和当前工具列表计算出一个 `HashMap<String, CredentialLookup>`，包装为闭包传入。hook 只负责查 map + 告警/Deny。

- [ ] **Step 1: 添加 tracing 依赖到 Cargo.toml**

在 `crates/llm-harness-strategy/Cargo.toml` 的 `[dependencies]` 中添加：

```toml
tracing.workspace = true
```

- [ ] **Step 2: 在 lib.rs 添加 module 声明**

在 `crates/llm-harness-strategy/src/lib.rs` 中添加：

```rust
pub mod credential_guard;
```

- [ ] **Step 3: 写 credential_guard.rs 完整文件（含测试）**

```rust
//! MCP credential scope guard hook.
//!
//! `McpCredentialGuardHook` implements `BeforeToolCallHook`. It checks whether
//! the current tool call is authorized under the MCP server's credential
//! scope declarations. By default it only logs a warning for unauthorized
//! tools; a `deny_unauthorized` flag can be set to `true` for fail-closed
//! mode.
//!
//! Due to MCP's process-level credential model (env/headers are shared across
//! all tools on a server), this hook provides detection, not enforcement.
//! True per-tool credential isolation would require separate transport
//! connections per tool, which is impractical.

use std::collections::HashMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use futures::FutureExt;
use llm_harness_types::{
    BeforeToolCallCtx, BeforeToolCallDecision, BeforeToolCallHook, ToolFailure,
};

/// Credential lookup result for a tool.
#[derive(Debug, Clone)]
pub enum CredentialLookup {
    /// Tool is not covered by any scoped credential declaration.
    /// No warning needed.
    NotScoped,
    /// Tool is authorized for the listed credential keys.
    Authorized(Vec<String>),
    /// Tool is on a server with scoped_env but not in any allowed_tools list.
    /// Warning or denial depending on policy.
    Unauthorized,
}

/// A lookup function that maps tool full_name to its credential status.
pub type CredentialLookupFn = Arc<dyn Fn(&str) -> CredentialLookup + Send + Sync>;

/// BeforeToolCallHook that warns (or denies) when a tool call is not
/// authorized under its server's credential scope declarations.
///
/// - `deny_unauthorized = false` (default): logs a warning, allows the call.
/// - `deny_unauthorized = true`: denies the call with a credential scope
///   violation error.
pub struct McpCredentialGuardHook {
    lookup: CredentialLookupFn,
    deny_unauthorized: bool,
}

impl McpCredentialGuardHook {
    /// Create a warning-only guard. Unauthorized tool calls are logged but allowed.
    pub fn warning(lookup: CredentialLookupFn) -> Self {
        Self {
            lookup,
            deny_unauthorized: false,
        }
    }

    /// Create a fail-closed guard. Unauthorized tool calls are denied.
    pub fn denying(lookup: CredentialLookupFn) -> Self {
        Self {
            lookup,
            deny_unauthorized: true,
        }
    }
}

impl BeforeToolCallHook for McpCredentialGuardHook {
    fn on_call<'a>(&'a self, ctx: BeforeToolCallCtx<'a>) -> BoxFuture<'a, BeforeToolCallDecision> {
        let result = (self.lookup)(ctx.tool_name);
        async move {
            match result {
                CredentialLookup::NotScoped | CredentialLookup::Authorized(_) => {
                    BeforeToolCallDecision::Allow
                }
                CredentialLookup::Unauthorized => {
                    if self.deny_unauthorized {
                        tracing::warn!(
                            tool = ctx.tool_name,
                            "denying MCP tool call: credential scope violation"
                        );
                        BeforeToolCallDecision::Deny(ToolFailure::new(
                            "credential_scope_violation",
                            "Tool call denied: this tool is not in any credential's allowed_tools list.",
                        ))
                    } else {
                        tracing::warn!(
                            tool = ctx.tool_name,
                            "MCP tool call not authorized by any credential scope (allowed with warning)"
                        );
                        BeforeToolCallDecision::Allow
                    }
                }
            }
        }
        .boxed()
    }
}

/// Builds a `CredentialLookupFn` from a map of tool full_name → authorized credential keys,
/// plus a set of tool full_names that are on servers with scoped_env but not in any
/// allowed_tools list.
pub fn build_credential_lookup(
    authorized: HashMap<String, Vec<String>>,
    unauthorized: std::collections::HashSet<String>,
) -> CredentialLookupFn {
    Arc::new(move |tool_name: &str| {
        if let Some(creds) = authorized.get(tool_name) {
            if creds.is_empty() {
                CredentialLookup::Unauthorized
            } else {
                CredentialLookup::Authorized(creds.clone())
            }
        } else if unauthorized.contains(tool_name) {
            CredentialLookup::Unauthorized
        } else {
            CredentialLookup::NotScoped
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::{AssistantMessage, RunContext};

    fn make_ctx<'a>(tool_name: &'a str) -> BeforeToolCallCtx<'a> {
        let run = RunContext::default();
        let assistant = AssistantMessage::default();
        BeforeToolCallCtx {
            run: &run,
            assistant_message: &assistant,
            tool_use_id: "test",
            tool_name,
            args: &serde_json::Value::Null,
            turn_index: 0,
        }
    }

    fn make_lookup() -> CredentialLookupFn {
        let mut authorized = HashMap::new();
        authorized.insert("mcp__srv_tool_a".to_owned(), vec!["TOKEN".to_owned()]);
        let mut unauthorized = std::collections::HashSet::new();
        unauthorized.insert("mcp__srv_tool_b".to_owned());
        build_credential_lookup(authorized, unauthorized)
    }

    #[tokio::test]
    async fn authorized_tool_allowed() {
        let hook = McpCredentialGuardHook::warning(make_lookup());
        let ctx = make_ctx("mcp__srv_tool_a");
        let decision = hook.on_call(ctx).await;
        assert!(matches!(decision, BeforeToolCallDecision::Allow));
    }

    #[tokio::test]
    async fn unauthorized_tool_warned_but_allowed() {
        let hook = McpCredentialGuardHook::warning(make_lookup());
        let ctx = make_ctx("mcp__srv_tool_b");
        let decision = hook.on_call(ctx).await;
        assert!(matches!(decision, BeforeToolCallDecision::Allow));
    }

    #[tokio::test]
    async fn unauthorized_tool_denied_in_deny_mode() {
        let hook = McpCredentialGuardHook::denying(make_lookup());
        let ctx = make_ctx("mcp__srv_tool_b");
        let decision = hook.on_call(ctx).await;
        assert!(matches!(decision, BeforeToolCallDecision::Deny(_)));
    }

    #[tokio::test]
    async fn not_scoped_tool_allowed() {
        let hook = McpCredentialGuardHook::warning(make_lookup());
        let ctx = make_ctx("some_other_tool");
        let decision = hook.on_call(ctx).await;
        assert!(matches!(decision, BeforeToolCallDecision::Allow));
    }
}
```

注意：测试中 `RunContext::default()` 和 `AssistantMessage::default()` 需要这些类型实现 `Default`。检查 `llm-harness-types` 中是否已实现。如果没有，需要在测试中手动构造或在该 crate 中为测试添加 `Default` impl。如果无法构造 `BeforeToolCallCtx`，则跳过集成测试，仅测试 `build_credential_lookup` 和 `CredentialLookup` 逻辑。

如果 `BeforeToolCallCtx` 无法在测试中构造，则用以下替代测试：

```rust
    #[test]
    fn build_lookup_authorizes_correctly() {
        let mut authorized = HashMap::new();
        authorized.insert("mcp__srv_tool_a".to_owned(), vec!["TOKEN".to_owned()]);
        let mut unauthorized = std::collections::HashSet::new();
        unauthorized.insert("mcp__srv_tool_b".to_owned());
        let lookup = build_credential_lookup(authorized, unauthorized);

        match lookup("mcp__srv_tool_a") {
            CredentialLookup::Authorized(creds) => assert_eq!(creds, vec!["TOKEN"]),
            _ => panic!("expected Authorized"),
        }
        match lookup("mcp__srv_tool_b") {
            CredentialLookup::Unauthorized => {}
            _ => panic!("expected Unauthorized"),
        }
        match lookup("other") {
            CredentialLookup::NotScoped => {}
            _ => panic!("expected NotScoped"),
        }
    }
```

- [ ] **Step 4: 运行测试验证通过**

Run: `cargo test -p llm-harness-strategy credential_guard::tests -- --nocapture`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/llm-harness-strategy/src/credential_guard.rs crates/llm-harness-strategy/src/lib.rs crates/llm-harness-strategy/Cargo.toml
git commit -m "feat(strategy): add McpCredentialGuardHook (BeforeToolCallHook)

Default warning-only mode logs when a tool call is not authorized by
any credential scope. Deny mode blocks the call. build_credential_lookup
helper constructs the lookup function from authorized/unauthorized tool sets."
```

---

## Self-Review

**1. Spec coverage:**
- 锁文件格式（spec 1.1）→ Task 1 ✅
- 锁文件读写（spec 1.1）→ Task 2 ✅
- 握手时版本锁定（spec 1.2）→ Task 7 ✅
- 运行时工具定义变更锁定（spec 1.3）→ Task 8 ✅
- VersionPolicy 配置接口（spec 1.4）→ Task 3 ✅
- 凭证引用语法（spec 2.1）→ Task 4 ✅
- CredentialProvider trait（spec 2.2）→ Task 4 ✅
- 凭证作用域声明（spec 2.3）→ Task 5 ✅
- 作用域强制——机制层拦截点（spec 2.4）→ Task 10 ✅
- 作用域强制——策略层 hook（spec 2.4）→ Task 11 ✅
- 错误类型（spec 3.2）→ Task 6 ✅
- 凭证解析集成到连接路径（spec 3.1）→ Task 9 ✅

**2. Placeholder scan:** 无 TBD/TODO。Task 11 的 `BeforeToolCallCtx` 构造问题有明确的降级方案（替代测试 `build_credential_lookup` 逻辑）。

**3. Type consistency:**
- `McpLockFile` / `ServerLockEntry` / `ToolSnapshotEntry` 在 Task 1 定义，Task 2/7 使用 ✅
- `VersionPolicy` / `MismatchPolicy` / `ToolChangePolicy` 在 Task 3 定义，Task 7 使用 ✅
- `CredentialProvider` / `EnvCredentialProvider` / `resolve_credential_value` / `resolve_map` 在 Task 4 定义，Task 9 使用 ✅
- `ScopedCredential` / `CredentialDeclarations` 在 Task 5 定义，Task 9/10 使用 ✅
- `McpManagerError` 新变体在 Task 6 定义，Task 7/9 使用 ✅
- `McpToolAdapter.authorized_credentials` 在 Task 10 定义 ✅
- `CredentialLookup` / `McpCredentialGuardHook` 在 Task 11 定义 ✅