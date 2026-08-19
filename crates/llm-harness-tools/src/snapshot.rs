//! In-memory whole-file snapshot store shared between `ReadTool` (writer)
//! and `EditTool` (reader). Uses 4-hex content-hash tags for stale detection.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use parking_lot::RwLock;
use sha2::{Digest, Sha256};

/// Maximum number of distinct paths retained by the store.
const MAX_PATHS: usize = 30;
/// Files larger than this (in bytes) are tagged but not snapshotted.
const SNAPSHOT_MAX_BYTES: usize = 4 * 1024 * 1024;

/// A recorded snapshot of a file: its content-hash tag, line contents, and
/// the mtime of the last access (used for LRU eviction).
#[derive(Debug, Clone)]
pub struct SnapshotEntry {
    /// 4-hex content-hash tag of the snapshotted lines.
    pub tag: String,
    /// The snapshotted lines, in order.
    pub lines: Vec<String>,
    /// Last-access time, used to drive LRU eviction.
    pub mtime: SystemTime,
}

/// An in-memory whole-file snapshot store.
///
/// The store is shared between `ReadTool` (which records snapshots) and
/// `EditTool` (which looks them up to detect stale edits via 4-hex
/// content-hash tags). Entries are evicted by oldest mtime once the store
/// reaches [`MAX_PATHS`] entries. Files exceeding [`SNAPSHOT_MAX_BYTES`]
/// are tagged but not stored.
pub struct FileSnapshotStore {
    snapshots: RwLock<HashMap<PathBuf, SnapshotEntry>>,
}

impl FileSnapshotStore {
    /// Create an empty snapshot store.
    pub fn new() -> Self {
        Self {
            snapshots: RwLock::new(HashMap::new()),
        }
    }

    /// Record a snapshot for `path` and return its 4-hex content-hash tag.
    ///
    /// Files larger than [`SNAPSHOT_MAX_BYTES`] are tagged but not stored.
    /// When the store is at capacity and a new path is added, the entry with
    /// the oldest mtime is evicted first.
    pub fn record(&self, path: &Path, lines: Vec<String>) -> String {
        let total_bytes: usize = lines.iter().map(|l| l.len() + 1).sum();
        let tag = compute_tag(&lines);
        if total_bytes > SNAPSHOT_MAX_BYTES {
            return tag;
        }
        let entry = SnapshotEntry {
            tag: tag.clone(),
            lines,
            mtime: SystemTime::now(),
        };
        let mut map = self.snapshots.write();
        if map.len() >= MAX_PATHS
            && !map.contains_key(path)
            && let Some(lru_key) = map
                .iter()
                .min_by_key(|(_, e)| e.mtime)
                .map(|(k, _)| k.clone())
        {
            map.remove(&lru_key);
        }
        map.insert(path.to_path_buf(), entry);
        tag
    }

    /// Look up the snapshot for `path`, refreshing its mtime on hit.
    pub fn lookup(&self, path: &Path) -> Option<SnapshotEntry> {
        let mut map = self.snapshots.write();
        if let Some(entry) = map.get_mut(path) {
            entry.mtime = SystemTime::now();
            return Some(entry.clone());
        }
        None
    }

    /// Drop any snapshot recorded for `path`.
    pub fn invalidate(&self, path: &Path) {
        self.snapshots.write().remove(path);
    }
}

impl Default for FileSnapshotStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute the 4-hex content-hash tag for `lines`.
///
/// Trailing `\r` characters are stripped from each line (normalizing CRLF
/// endings), the lines are joined with `\n`, and the SHA-256 digest of the
/// result is taken. The first 2 bytes of the digest render as 4 hex chars.
pub fn compute_tag(lines: &[String]) -> String {
    let normalized: Vec<&str> = lines.iter().map(|l| l.trim_end_matches('\r')).collect();
    let joined = normalized.join("\n");
    let mut hasher = Sha256::new();
    hasher.update(joined.as_bytes());
    let result = hasher.finalize();
    result[..2].iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_is_deterministic() {
        let lines = vec!["hello".to_string(), "world".to_string()];
        assert_eq!(compute_tag(&lines), compute_tag(&lines));
        assert_eq!(compute_tag(&lines).len(), 4);
    }

    #[test]
    fn tag_changes_on_content_change() {
        let a = vec!["hello".to_string()];
        let b = vec!["goodbye".to_string()];
        assert_ne!(compute_tag(&a), compute_tag(&b));
    }

    #[test]
    fn tag_ignores_trailing_cr() {
        let a = vec!["hello".to_string()];
        let b = vec!["hello\r".to_string()];
        assert_eq!(compute_tag(&a), compute_tag(&b));
    }

    #[test]
    fn record_and_lookup() {
        let store = FileSnapshotStore::new();
        let path = Path::new("/tmp/example.txt");
        let lines = vec!["foo".to_string(), "bar".to_string()];
        let tag = store.record(path, lines.clone());
        let entry = store.lookup(path).expect("entry should be present");
        assert_eq!(entry.tag, tag);
        assert_eq!(entry.lines, lines);
    }

    #[test]
    fn invalidate_drops_entry() {
        let store = FileSnapshotStore::new();
        let path = Path::new("/tmp/invalidate.txt");
        store.record(path, vec!["x".to_string()]);
        assert!(store.lookup(path).is_some());
        store.invalidate(path);
        assert!(store.lookup(path).is_none());
    }

    #[test]
    fn lru_eviction_at_capacity() {
        let store = FileSnapshotStore::new();
        let first = Path::new("/tmp/lru/0.txt");
        store.record(first, vec!["first".to_string()]);
        // Fill the rest so the store reaches MAX_PATHS; the oldest entry
        // (`first`) should be evicted when we add one more new path.
        for i in 1..MAX_PATHS {
            let path = PathBuf::from(format!("/tmp/lru/{i}.txt"));
            store.record(&path, vec![i.to_string()]);
        }
        assert_eq!(store.snapshots.read().len(), MAX_PATHS);
        let extra = Path::new("/tmp/lru/extra.txt");
        store.record(extra, vec!["extra".to_string()]);
        let map = store.snapshots.read();
        assert_eq!(map.len(), MAX_PATHS);
        assert!(!map.contains_key(first), "oldest entry should be evicted");
        assert!(map.contains_key(extra), "new entry should be stored");
    }

    #[test]
    fn large_file_not_snapshotted() {
        let store = FileSnapshotStore::new();
        let path = Path::new("/tmp/large.txt");
        // One line just over SNAPSHOT_MAX_BYTES (each line costs len + 1).
        let big_line = "a".repeat(SNAPSHOT_MAX_BYTES);
        let lines = vec![big_line];
        let tag = store.record(path, lines);
        assert_eq!(tag.len(), 4);
        assert!(
            store.lookup(path).is_none(),
            "oversized file should not be snapshotted"
        );
    }
}
