use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::McpToolDefinition;

/// Root structure of the MCP lock file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpLockFile {
    pub version: u32,
    pub servers: std::collections::HashMap<String, ServerLockEntry>,
}

impl Default for McpLockFile {
    fn default() -> Self {
        Self {
            version: 1,
            servers: std::collections::HashMap::new(),
        }
    }
}

/// Lock entry for a single MCP server.
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

/// Hash snapshot for a single tool definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSnapshotEntry {
    pub name: String,
    pub description_hash: String,
    pub input_schema_hash: String,
}

/// Diff result of tool definition changes.
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

/// Compute hash snapshots for a set of tool definitions.
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

/// Compare locked tool hashes against current tool hashes, returning a diff.
pub fn diff_tool_snapshots(
    locked: &[ToolSnapshotEntry],
    current_hashes: &[ToolSnapshotEntry],
) -> ToolDiff {
    let current_map: std::collections::HashMap<&str, &ToolSnapshotEntry> = current_hashes
        .iter()
        .map(|e| (e.name.as_str(), e))
        .collect();
    let locked_map: std::collections::HashMap<&str, &ToolSnapshotEntry> =
        locked.iter().map(|e| (e.name.as_str(), e)).collect();

    let mut diff = ToolDiff::default();
    for entry in locked {
        match current_map.get(entry.name.as_str()) {
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
    for entry in current_hashes {
        if !locked_map.contains_key(entry.name.as_str()) {
            diff.added.push(entry.name.clone());
        }
    }
    diff.added.sort();
    diff
}

impl McpLockFile {
    pub async fn load_from_disk(path: &std::path::Path) -> anyhow::Result<Self> {
        match tokio::fs::read_to_string(path).await {
            Ok(text) => {
                let lock: Self = serde_json::from_str(&text)
                    .map_err(|e| anyhow::anyhow!("invalid lock file '{}': {e}", path.display()))?;
                Ok(lock)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(anyhow::anyhow!(
                "failed to read lock file '{}': {e}",
                path.display()
            )),
        }
    }

    pub async fn save_to_disk(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        let text = serde_json::to_string_pretty(self)?;
        // Write-then-rename keeps readers from observing a truncated file if
        // the process is interrupted or two managers persist the same path
        // concurrently. The unique suffix avoids temp-file collisions between
        // managers in the same process.
        static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let suffix = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp-{}-{suffix}", std::process::id()));
        if let Err(error) = tokio::fs::write(&tmp, text).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(error.into());
        }
        if let Err(error) = tokio::fs::rename(&tmp, path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(error.into());
        }
        Ok(())
    }

    pub fn get_entry(&self, server_name: &str) -> Option<&ServerLockEntry> {
        self.servers.get(server_name)
    }

    pub fn upsert_entry(&mut self, server_name: String, entry: ServerLockEntry) {
        self.servers.insert(server_name, entry);
    }
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
        let current = compute_tool_hashes(&[tool("a", "desc"), tool("b", "desc")]);
        let diff = diff_tool_snapshots(&locked, &current);
        assert_eq!(diff.added, vec!["b"]);
        assert!(diff.removed.is_empty());
        assert!(diff.modified_desc.is_empty());
        assert!(diff.modified_schema.is_empty());
    }

    #[test]
    fn diff_detects_removed_tool() {
        let locked = compute_tool_hashes(&[tool("a", "desc"), tool("b", "desc")]);
        let current = compute_tool_hashes(&[tool("a", "desc")]);
        let diff = diff_tool_snapshots(&locked, &current);
        assert_eq!(diff.removed, vec!["b"]);
        assert!(diff.added.is_empty());
    }

    #[test]
    fn diff_detects_modified_description() {
        let locked = compute_tool_hashes(&[tool("a", "original")]);
        let current = compute_tool_hashes(&[tool("a", "poisoned")]);
        let diff = diff_tool_snapshots(&locked, &current);
        assert_eq!(diff.modified_desc, vec!["a"]);
        assert!(diff.modified_schema.is_empty());
    }

    #[test]
    fn diff_detects_modified_schema() {
        let locked = compute_tool_hashes(&[tool("a", "desc")]);
        let mut changed = tool("a", "desc");
        changed.input_schema = json!({"type": "object", "properties": {"q": {"type": "string"}}});
        let current = compute_tool_hashes(&[changed]);
        let diff = diff_tool_snapshots(&locked, &current);
        assert!(diff.modified_desc.is_empty());
        assert_eq!(diff.modified_schema, vec!["a"]);
    }

    #[test]
    fn diff_empty_when_no_changes() {
        let locked = compute_tool_hashes(&[tool("a", "desc")]);
        let current = compute_tool_hashes(&[tool("a", "desc")]);
        let diff = diff_tool_snapshots(&locked, &current);
        assert!(diff.is_empty());
    }
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
        let lock = McpLockFile::load_from_disk(path).await.unwrap();
        assert!(lock.servers.is_empty());
    }
}
