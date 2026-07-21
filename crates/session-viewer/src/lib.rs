//! Session viewer: reads on-disk session logs produced by `JsonlSessionRepo`
//! and renders them as a self-contained HTML page.
//!
//! This crate depends only on the on-disk JSON format (`meta.json` +
//! `entries.jsonl`), NOT on any runtime crate. Any agent application built on
//! Senza or `llm-harness-runtime` that persists sessions via `JsonlSessionRepo`
//! produces files this viewer can read.
//!
//! Two entry points:
//! - [`read_sessions`] — read a sessions directory into [`ViewerData`] (JSON).
//! - [`serve`] — start a local HTTP server and open the viewer in a browser.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

mod html;

pub use html::VIEWER_HTML;

// ── On-disk types (mirror of llm-harness-agent session types) ────────────────
// These intentionally re-declare the on-disk JSON shapes rather than depending
// on llm-harness-types, so this crate stays standalone and buildable by
// downstreams that do not pull in the runtime.

/// `meta.json` — session metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub active_cursor: Option<String>,
    #[serde(default)]
    pub parent_session_path: Option<String>,
}

/// One entry in `entries.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub payload: serde_json::Value,
}

// ── Viewer output model ───────────────────────────────────────────────────────

/// Everything the HTML viewer needs, serialized as JSON and embedded in the page.
#[derive(Debug, Clone, Serialize)]
pub struct ViewerData {
    /// Root directory the viewer was pointed at (for display).
    pub root: String,
    /// One item per session directory found under root.
    pub sessions: Vec<SessionData>,
}

/// A single session: metadata + all entries + precomputed tree structure.
#[derive(Debug, Clone, Serialize)]
pub struct SessionData {
    pub meta: SessionMeta,
    /// All entries in storage order (as they appear in entries.jsonl).
    pub entries: Vec<SessionEntry>,
    /// Precomputed branches: each is a root-first path of entry IDs ending at a leaf.
    pub branches: Vec<Branch>,
    /// Index (into `entries`) of the entry the active_cursor points at, if any.
    pub active_index: Option<usize>,
}

/// A branch (root → leaf path).
#[derive(Debug, Clone, Serialize)]
pub struct Branch {
    /// Root-first entry IDs on this branch.
    pub path: Vec<String>,
    /// True if this branch contains the active cursor.
    pub is_active: bool,
    /// Optional label (from a `label` payload on the leaf, if present).
    pub label: Option<String>,
}

// ── Reading ───────────────────────────────────────────────────────────────────

/// Read all sessions under `root`.
///
/// `root` may be either:
/// - a sessions root containing `<session_id>/` subdirectories (e.g.
///   `.../sessions/sub-agents/`), or
/// - a single session directory containing `meta.json` + `entries.jsonl`.
pub fn read_sessions(root: &Path) -> Result<ViewerData> {
    let root = root.canonicalize().context("canonicalize root")?;

    // Single session dir: contains meta.json directly.
    if root.join("meta.json").is_file() {
        let sd = read_one_session(&root)?;
        return Ok(ViewerData {
            root: root.display().to_string(),
            sessions: vec![sd],
        });
    }

    // Sessions root: scan subdirectories.
    let mut sessions = Vec::new();
    for entry in std::fs::read_dir(&root).with_context(|| format!("read_dir {}", root.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir = entry.path();
        if !dir.join("meta.json").is_file() {
            continue;
        }
        match read_one_session(&dir) {
            Ok(sd) => sessions.push(sd),
            Err(e) => {
                // Skip unreadable sessions rather than failing the whole viewer.
                tracing_warn(&format!("skip session {}: {e}", dir.display()));
            }
        }
    }
    // Most recently updated first.
    sessions.sort_by_key(|b| std::cmp::Reverse(b.meta.updated_at));
    Ok(ViewerData {
        root: root.display().to_string(),
        sessions,
    })
}

fn read_one_session(dir: &Path) -> Result<SessionData> {
    let meta_bytes = std::fs::read(dir.join("meta.json"))
        .with_context(|| format!("read meta.json in {}", dir.display()))?;
    let meta: SessionMeta = serde_json::from_slice(&meta_bytes)
        .with_context(|| format!("parse meta.json in {}", dir.display()))?;

    let entries_path = dir.join("entries.jsonl");
    let entries = if entries_path.is_file() {
        read_entries(&entries_path)?
    } else {
        Vec::new()
    };

    let branches = compute_branches(&entries, &meta);
    let active_index = meta
        .active_cursor
        .as_ref()
        .and_then(|cursor| entries.iter().position(|e| &e.id == cursor));

    Ok(SessionData {
        meta,
        entries,
        branches,
        active_index,
    })
}

fn read_entries(path: &Path) -> Result<Vec<SessionEntry>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut entries = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: SessionEntry = serde_json::from_str(line)
            .with_context(|| format!("parse {} line {}", path.display(), lineno + 1))?;
        entries.push(entry);
    }
    Ok(entries)
}

/// Compute all branches (root → leaf paths) from the entry list.
fn compute_branches(entries: &[SessionEntry], _meta: &SessionMeta) -> Vec<Branch> {
    use std::collections::HashMap;

    // id → entry index
    let mut by_id: HashMap<&str, usize> = HashMap::new();
    // parent_id (as str, "ROOT" for None) → child indices
    let mut children: HashMap<&str, Vec<usize>> = HashMap::new();

    for (i, e) in entries.iter().enumerate() {
        by_id.insert(e.id.as_str(), i);
        let key = e.parent_id.as_deref().unwrap_or("ROOT");
        children.entry(key).or_default().push(i);
    }

    // Leaves = entries that are nobody's parent.
    let is_parent_of_someone = |id: &str| children.contains_key(id);
    let mut branches = Vec::new();
    for e in entries.iter() {
        if !is_parent_of_someone(e.id.as_str()) {
            // Walk root-first.
            let mut path = vec![e.id.clone()];
            let mut cur = e;
            while let Some(pid) = cur.parent_id.as_deref() {
                if let Some(&pi) = by_id.get(pid) {
                    path.push(pid.to_string());
                    cur = &entries[pi];
                } else {
                    break;
                }
            }
            path.reverse();
            let is_active = _meta.active_cursor.as_deref() == Some(e.id.as_str());
            // Label: look for a `label` payload on the leaf.
            let label = leaf_label(&e.payload);
            branches.push(Branch {
                path,
                is_active,
                label,
            });
        }
    }
    branches
}

fn leaf_label(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("entry_type")
        .and_then(|v| v.as_str())
        .and_then(|t| (t == "label").then_some(()))
        .and_then(|()| {
            payload
                .get("name")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
}

// Minimal stand-in for tracing::warn (avoid pulling in tracing for a viewer).
fn tracing_warn(msg: &str) {
    eprintln!("warning: {msg}");
}

// ── Serving ───────────────────────────────────────────────────────────────────

/// Start a local HTTP server serving the viewer for `root` and open a browser.
///
/// Blocks until interrupted. Picks the first free port in 127.0.0.1.
pub async fn serve(root: &Path) -> Result<()> {
    serve_on(root, 0).await
}

/// Like [`serve`] but binds a specific port (0 = ephemeral).
pub async fn serve_on(root: &Path, port: u16) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let data = read_sessions(root)?;
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let local_port = listener.local_addr()?.port();
    let url = format!("http://127.0.0.1:{local_port}");
    println!(
        "session-viewer serving {root} at {url}",
        root = root.display()
    );

    // Best-effort browser open; ignore failures.
    let _ = open_browser(&url);

    loop {
        let (mut sock, _) = listener.accept().await?;
        let data_json = serde_json::to_string(&data)?;
        // Inline the HTML with the data embedded. We serve the same page for
        // every request; the data is fixed at startup.
        let html = render_page(&data_json);
        let body = html.into_bytes();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        if sock.write_all(resp.as_bytes()).await.is_err() {
            continue;
        }
        let _ = sock.write_all(&body).await;
        let _ = sock.flush().await;
        // Drain & close.
        let mut buf = [0u8; 1024];
        let _ = sock.read(&mut buf).await;
    }
}

/// Build a self-contained HTML page with `data_json` embedded.
pub fn render_page(data_json: &str) -> String {
    // VIEWER_HTML contains a `<script id="viewer-data" type="application/json">…</script>`
    // placeholder; we splice the data in.
    VIEWER_HTML.replace("__VIEWER_DATA_JSON__", data_json)
}

#[cfg(unix)]
fn open_browser(url: &str) -> Result<()> {
    std::process::Command::new("open").arg(url).status()?;
    Ok(())
}

#[cfg(not(unix))]
fn open_browser(url: &str) -> Result<()> {
    // Windows / other: best effort.
    std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .status()?;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_session(dir: &Path, meta: &SessionMeta, entries: &[&str]) {
        fs::write(
            dir.join("meta.json"),
            serde_json::to_vec_pretty(meta).unwrap(),
        )
        .unwrap();
        let mut text = String::new();
        for e in entries {
            text.push_str(e);
            text.push('\n');
        }
        fs::write(dir.join("entries.jsonl"), text).unwrap();
    }

    #[test]
    fn reads_single_session_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = SessionMeta {
            id: "s1".into(),
            name: Some("test".into()),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            model: Some("gpt-4o".into()),
            active_cursor: None,
            parent_session_path: None,
        };
        write_session(tmp.path(), &meta, &[]);
        let data = read_sessions(tmp.path()).unwrap();
        assert_eq!(data.sessions.len(), 1);
        assert_eq!(data.sessions[0].meta.id, "s1");
        assert!(data.sessions[0].entries.is_empty());
    }

    #[test]
    fn reads_sessions_root_with_multiple_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let mk = |id: &str, updated: chrono::DateTime<chrono::Utc>| {
            let dir = tmp.path().join(id);
            fs::create_dir_all(&dir).unwrap();
            let meta = SessionMeta {
                id: id.into(),
                name: None,
                created_at: updated,
                updated_at: updated,
                model: None,
                active_cursor: None,
                parent_session_path: None,
            };
            write_session(&dir, &meta, &[]);
        };
        let t0 = chrono::Utc::now();
        mk("a", t0);
        mk("b", t0 + chrono::Duration::seconds(1));
        let data = read_sessions(tmp.path()).unwrap();
        assert_eq!(data.sessions.len(), 2);
        // Sorted by updated_at desc.
        assert_eq!(data.sessions[0].meta.id, "b");
        assert_eq!(data.sessions[1].meta.id, "a");
    }

    #[test]
    fn computes_branches_for_tree() {
        // root(e1) -> e2 -> e3 (leaf)
        //                \-> e4 (leaf)
        let entries = [
            r#"{"id":"e1","parent_id":null,"timestamp":"2026-01-01T00:00:00Z","payload":{"entry_type":"message","role":"user","content":[],"timestamp":"2026-01-01T00:00:00Z"}}"#,
            r#"{"id":"e2","parent_id":"e1","timestamp":"2026-01-01T00:00:01Z","payload":{"entry_type":"message","role":"assistant","content":[],"timestamp":"2026-01-01T00:00:01Z"}}"#,
            r#"{"id":"e3","parent_id":"e2","timestamp":"2026-01-01T00:00:02Z","payload":{"entry_type":"message","role":"user","content":[],"timestamp":"2026-01-01T00:00:02Z"}}"#,
            r#"{"id":"e4","parent_id":"e2","timestamp":"2026-01-01T00:00:03Z","payload":{"entry_type":"message","role":"user","content":[],"timestamp":"2026-01-01T00:00:03Z"}}"#,
        ];
        let entries: Vec<SessionEntry> = entries
            .iter()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        let meta = SessionMeta {
            id: "s".into(),
            name: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            model: None,
            active_cursor: Some("e3".into()),
            parent_session_path: None,
        };
        let branches = compute_branches(&entries, &meta);
        assert_eq!(branches.len(), 2);
        // e3 branch is active.
        let active = branches.iter().find(|b| b.is_active).unwrap();
        assert_eq!(active.path, vec!["e1", "e2", "e3"]);
        // e4 branch.
        let other = branches.iter().find(|b| !b.is_active).unwrap();
        assert_eq!(other.path, vec!["e1", "e2", "e4"]);
    }

    #[test]
    fn skips_unreadable_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        // Good session.
        let good = tmp.path().join("good");
        fs::create_dir_all(&good).unwrap();
        let meta = SessionMeta {
            id: "good".into(),
            name: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            model: None,
            active_cursor: None,
            parent_session_path: None,
        };
        write_session(&good, &meta, &[]);
        // Bad session: meta.json exists but is invalid JSON.
        let bad = tmp.path().join("bad");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("meta.json"), "{not json").unwrap();
        let data = read_sessions(tmp.path()).unwrap();
        assert_eq!(data.sessions.len(), 1);
        assert_eq!(data.sessions[0].meta.id, "good");
    }

    #[test]
    fn render_page_contains_data() {
        let data = ViewerData {
            root: "/tmp/x".into(),
            sessions: vec![],
        };
        let json = serde_json::to_string(&data).unwrap();
        let html = render_page(&json);
        assert!(html.contains(&json));
        assert!(html.contains("viewer-data"));
    }
}
