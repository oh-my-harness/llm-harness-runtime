use std::path::PathBuf;

use futures::future::BoxFuture;
use sha2::{Digest, Sha256};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use anyhow::Result;
use llm_harness_runtime::observability::audit::{AuditEntry, AuditSink};

/// JSONL file-backed audit sink with SHA-256 hash-chain integrity.
///
/// Each entry is written as one JSON line. Hash is computed over all fields
/// except `hash` itself:
///   `hash_n = SHA256(json_without_hash_field + ":" + prev_hash)` for n > 0
///   `hash_0 = SHA256(json_without_hash_field)` for first entry
///
/// Tampering is detectable by `validate()`.
pub struct JsonlAuditSink {
    path: PathBuf,
    /// tokio::sync::Mutex — allows holding the lock across .await during writes.
    prev_hash: Mutex<Option<String>>,
    /// File handle, lazily opened on first write.
    file: Mutex<Option<tokio::fs::File>>,
}

impl JsonlAuditSink {
    /// Create a new sink that writes to the given path (opened lazily).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            prev_hash: Mutex::new(None),
            file: Mutex::new(None),
        }
    }

    fn compute_hash(content: &str, prev: Option<&str>) -> String {
        let input = match prev {
            Some(p) => format!("{content}:{p}"),
            None => content.to_string(),
        };
        let bytes = Sha256::digest(input.as_bytes());
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Validate the hash chain of a JSONL audit log.
    ///
    /// Returns the number of valid entries, or an error on the first broken link.
    pub async fn validate(path: &std::path::Path) -> Result<usize> {
        let raw = tokio::fs::read_to_string(path).await?;
        let mut count = 0;
        let mut prev_hash: Option<String> = None;

        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let mut val: serde_json::Value = serde_json::from_str(line)
                .map_err(|e| anyhow::anyhow!("parse error at line {count}: {e}"))?;

            let stored = val["hash"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing hash at line {count}"))?
                .to_string();

            val.as_object_mut().unwrap().remove("hash");
            let content_str = serde_json::to_string(&val)?;
            let computed = Self::compute_hash(&content_str, prev_hash.as_deref());

            if computed != stored {
                return Err(anyhow::anyhow!(
                    "hash chain broken at line {count}: expected {stored}, computed {computed}"
                ));
            }

            prev_hash = Some(computed);
            count += 1;
        }

        Ok(count)
    }
}

impl AuditSink for JsonlAuditSink {
    fn record(&self, mut entry: AuditEntry) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            // Hold prev_hash lock for the full write to ensure serial hash chain.
            let mut prev_guard = self.prev_hash.lock().await;
            let prev = prev_guard.clone();

            // Set prev_hash; clear hash before computing.
            entry.prev_hash = prev.clone();
            entry.hash = String::new();

            // Serialize entry, strip hash field, compute hash.
            let mut val = serde_json::to_value(&entry)?;
            val.as_object_mut().unwrap().remove("hash");
            let content_str = serde_json::to_string(&val)?;
            let hash = Self::compute_hash(&content_str, prev.as_deref());

            // Set computed hash and write final line.
            entry.hash = hash.clone();
            let line = serde_json::to_string(&entry)?;

            let mut file_guard = self.file.lock().await;
            if file_guard.is_none() {
                let f = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)
                    .await?;
                *file_guard = Some(f);
            }
            let file = file_guard.as_mut().unwrap();
            file.write_all(line.as_bytes()).await?;
            file.write_all(b"\n").await?;
            file.flush().await?;
            drop(file_guard);

            *prev_guard = Some(hash);
            Ok(())
        })
    }

    fn flush(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if let Some(ref mut f) = *self.file.lock().await {
                f.flush().await?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_runtime::observability::audit::{AuditDecision, AuditEventType};
    use llm_harness_types::EntryId;

    fn make_entry(trace_id: &str) -> AuditEntry {
        AuditEntry {
            timestamp: chrono::Utc::now(),
            trace_id: trace_id.into(),
            session_id: EntryId::new(),
            task_id: None,
            principal: "agent".into(),
            event_type: AuditEventType::ToolCall,
            payload: serde_json::json!({"tool": "read_file"}),
            decision: Some(AuditDecision::Allow),
            cost_delta: None,
            prev_hash: None,
            hash: String::new(),
        }
    }

    #[tokio::test]
    async fn write_and_validate_hash_chain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");

        let sink = JsonlAuditSink::new(&path);
        sink.record(make_entry("trace-1")).await.unwrap();
        sink.record(make_entry("trace-2")).await.unwrap();
        sink.record(make_entry("trace-3")).await.unwrap();
        sink.flush().await.unwrap();

        let count = JsonlAuditSink::validate(&path).await.unwrap();
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn tampered_log_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");

        let sink = JsonlAuditSink::new(&path);
        sink.record(make_entry("trace-1")).await.unwrap();
        sink.record(make_entry("trace-2")).await.unwrap();
        sink.flush().await.unwrap();

        let mut content = tokio::fs::read_to_string(&path).await.unwrap();
        content = content.replace("read_file", "rm_-rf");
        tokio::fs::write(&path, content).await.unwrap();

        let result = JsonlAuditSink::validate(&path).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("hash chain broken")
        );
    }

    #[tokio::test]
    async fn empty_log_validates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.jsonl");
        tokio::fs::write(&path, "").await.unwrap();

        let count = JsonlAuditSink::validate(&path).await.unwrap();
        assert_eq!(count, 0);
    }
}
