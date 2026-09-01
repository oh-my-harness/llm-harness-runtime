//! Markdown-file issue store.
//!
//! One issue = one `.md` file with a fixed four-line frontmatter
//! (`id`/`status`/`source`/`created`/`title`); everything after the closing
//! `---` is the body. State transitions are validated here; WHO may
//! transition (only the human, via the panel or by editing the file) is
//! enforced by callers — agents never receive a `set_status` entry point.

use std::path::{Path, PathBuf};

/// Lifecycle of an issue. Only the human transitions states:
/// `pending → confirmed → done`, or `pending → rejected`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueStatus {
    Pending,
    Confirmed,
    Rejected,
    Done,
}

impl IssueStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Confirmed => "confirmed",
            Self::Rejected => "rejected",
            Self::Done => "done",
        }
    }

    /// Whether `self → to` is a legal transition.
    fn can_transition_to(self, to: IssueStatus) -> bool {
        use IssueStatus::*;
        matches!(
            (self, to),
            (Pending, Confirmed) | (Pending, Rejected) | (Confirmed, Done)
        )
    }
}

impl std::str::FromStr for IssueStatus {
    type Err = IssueError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "pending" => Ok(Self::Pending),
            "confirmed" => Ok(Self::Confirmed),
            "rejected" => Ok(Self::Rejected),
            "done" => Ok(Self::Done),
            other => Err(IssueError::Parse(format!("unknown status: {other}"))),
        }
    }
}

impl std::fmt::Display for IssueStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct Issue {
    pub id: String,
    pub status: IssueStatus,
    pub source: String,
    pub created: String,
    pub title: String,
    pub body: String,
}

#[derive(Debug, thiserror::Error)]
pub enum IssueError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse: {0}")]
    Parse(String),
    #[error("invalid transition: {from} -> {to}")]
    InvalidTransition { from: IssueStatus, to: IssueStatus },
    #[error("issue not found: {0}")]
    NotFound(String),
}

/// Store rooted at a directory; one file per issue.
#[derive(Debug, Clone)]
pub struct IssueStore {
    root: PathBuf,
}

impl IssueStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The store directory (for isolation assertions and scout task text).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create a new pending issue, assigning the next sequential `I-NNN` id.
    pub fn create(&self, source: &str, title: &str, body: &str) -> Result<Issue, IssueError> {
        std::fs::create_dir_all(&self.root)?;
        let next = self.next_id()?;
        let issue = Issue {
            id: format!("I-{next:03}"),
            status: IssueStatus::Pending,
            source: source.to_string(),
            created: chrono::Utc::now().to_rfc3339(),
            title: title.to_string(),
            body: body.to_string(),
        };
        let path = self.issue_path(&issue.id, &issue.title);
        std::fs::write(&path, render(&issue))?;
        Ok(issue)
    }

    /// List all issues (sorted by id).
    pub fn list(&self) -> Result<Vec<Issue>, IssueError> {
        let mut issues = Vec::new();
        if !self.root.exists() {
            return Ok(issues);
        }
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let text = std::fs::read_to_string(&path)?;
            issues.push(parse(&text).map_err(|e| {
                IssueError::Parse(format!(
                    "{}: {e}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ))
            })?);
        }
        issues.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(issues)
    }

    /// Count issues currently in `status`.
    pub fn count_in(&self, status: IssueStatus) -> Result<usize, IssueError> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|i| i.status == status)
            .count())
    }

    /// Transition an issue's status (human-only operation; callers enforce).
    pub fn set_status(&self, id: &str, to: IssueStatus) -> Result<Issue, IssueError> {
        let (issue, path) = self.load(id)?;
        if !issue.status.can_transition_to(to) {
            return Err(IssueError::InvalidTransition {
                from: issue.status,
                to,
            });
        }
        let mut updated = issue;
        updated.status = to;
        // Atomic write: temp file + rename.
        let tmp = path.with_extension("md.tmp");
        std::fs::write(&tmp, render(&updated))?;
        std::fs::rename(&tmp, &path)?;
        Ok(updated)
    }

    fn next_id(&self) -> Result<u32, IssueError> {
        let mut max = 0u32;
        if self.root.exists() {
            for entry in std::fs::read_dir(&self.root)? {
                let name = entry?.file_name();
                let name = name.to_string_lossy();
                if let Some(id) = name.strip_prefix("I-")
                    && let Some(num) = id.split('-').next()
                    && let Ok(n) = num.parse::<u32>()
                {
                    max = max.max(n);
                }
            }
        }
        Ok(max + 1)
    }

    fn issue_path(&self, id: &str, title: &str) -> PathBuf {
        self.root.join(format!("{id}-{}.md", slug(title)))
    }

    /// Locate the file for `id` regardless of its title slug, returning the
    /// parsed issue and its path.
    pub fn load(&self, id: &str) -> Result<(Issue, PathBuf), IssueError> {
        if !self.root.exists() {
            return Err(IssueError::NotFound(id.to_string()));
        }
        for entry in std::fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with(&format!("{id}-")) || name == format!("{id}.md") {
                let text = std::fs::read_to_string(&path)?;
                return Ok((parse(&text)?, path));
            }
        }
        Err(IssueError::NotFound(id.to_string()))
    }
}

/// Filename slug: lowercase, non-alphanumeric folded to `-`, truncated 40.
fn slug(title: &str) -> String {
    let mut s = String::new();
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            s.push(ch.to_ascii_lowercase());
        } else if !s.ends_with('-') {
            s.push('-');
        }
        if s.len() >= 40 {
            break;
        }
    }
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "issue".to_string() } else { s }
}

/// Render an issue to its on-disk Markdown form.
fn render(issue: &Issue) -> String {
    format!(
        "---\nid: {}\nstatus: {}\nsource: {}\ncreated: {}\ntitle: {}\n---\n{}\n",
        issue.id, issue.status, issue.source, issue.created, issue.title, issue.body
    )
}

/// Parse the fixed frontmatter format: `---` line, five `key: value` lines,
/// closing `---`, then the body. Lenient about leading blank lines.
fn parse(text: &str) -> Result<Issue, IssueError> {
    let all: Vec<&str> = text.lines().collect();
    let start = all.iter().position(|l| !l.trim().is_empty()).unwrap_or(0);
    if all.get(start).map(|l| l.trim()) != Some("---") {
        return Err(IssueError::Parse("missing frontmatter open".into()));
    }
    let mut id = None;
    let mut status = None;
    let mut source = None;
    let mut created = None;
    let mut title = None;
    let mut close = None;
    for (idx, line) in all[start + 1..].iter().enumerate() {
        if line.trim() == "---" {
            close = Some(start + 1 + idx);
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(IssueError::Parse(format!("bad frontmatter line: {line}")));
        };
        let value = value.trim();
        match key.trim() {
            "id" => id = Some(value.to_string()),
            "status" => status = Some(value.parse()?),
            "source" => source = Some(value.to_string()),
            "created" => created = Some(value.to_string()),
            "title" => title = Some(value.to_string()),
            other => return Err(IssueError::Parse(format!("unknown key: {other}"))),
        }
    }
    let Some(close) = close else {
        return Err(IssueError::Parse("missing frontmatter close".into()));
    };
    let Some(id) = id else {
        return Err(IssueError::Parse("missing id".into()));
    };
    let Some(status) = status else {
        return Err(IssueError::Parse("missing status".into()));
    };
    let Some(source) = source else {
        return Err(IssueError::Parse("missing source".into()));
    };
    let Some(created) = created else {
        return Err(IssueError::Parse("missing created".into()));
    };
    let Some(title) = title else {
        return Err(IssueError::Parse("missing title".into()));
    };
    let body = all[close + 1..].join("\n").trim().to_string();
    Ok(Issue {
        id,
        status,
        source,
        created,
        title,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> IssueStore {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ct-issue-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        IssueStore::new(dir)
    }

    #[test]
    fn create_assigns_sequential_ids_and_roundtrips() {
        let store = temp_store();
        let a = store
            .create("scout", "Fix io error context", "body a")
            .unwrap();
        let b = store.create("scout", "Second issue", "body b").unwrap();
        assert_eq!(a.id, "I-001");
        assert_eq!(b.id, "I-002");
        assert_eq!(a.status, IssueStatus::Pending);
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, "I-001");
    }

    #[test]
    fn pending_to_confirmed_to_done_ok() {
        let store = temp_store();
        let i = store.create("scout", "t", "b").unwrap();
        let c = store.set_status(&i.id, IssueStatus::Confirmed).unwrap();
        assert_eq!(c.status, IssueStatus::Confirmed);
        let d = store.set_status(&i.id, IssueStatus::Done).unwrap();
        assert_eq!(d.status, IssueStatus::Done);
    }

    #[test]
    fn invalid_transition_rejected() {
        let store = temp_store();
        let i = store.create("scout", "t", "b").unwrap();
        assert!(matches!(
            store.set_status(&i.id, IssueStatus::Done),
            Err(IssueError::InvalidTransition { .. })
        ));
        // rejected → confirmed also illegal.
        store.set_status(&i.id, IssueStatus::Rejected).unwrap();
        assert!(matches!(
            store.set_status(&i.id, IssueStatus::Confirmed),
            Err(IssueError::InvalidTransition { .. })
        ));
    }

    #[test]
    fn frontmatter_roundtrip_preserves_body() {
        let store = temp_store();
        let i = store
            .create("scout", "Title with spaces", "## 现象\n- a\n- b")
            .unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed[0].body, "## 现象\n- a\n- b");
        assert_eq!(listed[0].title, "Title with spaces");
        assert_eq!(listed[0].source, "scout");
        assert!(!listed[0].created.is_empty());
        let _ = i;
    }

    #[test]
    fn slug_folds_and_truncates() {
        assert_eq!(slug("Fix io error context!"), "fix-io-error-context");
        assert_eq!(slug("中文标题 also works"), "also-works");
        let long = slug(&"x".repeat(100));
        assert!(long.len() <= 40);
    }

    #[test]
    fn set_status_unknown_id() {
        let store = temp_store();
        assert!(matches!(
            store.set_status("I-999", IssueStatus::Confirmed),
            Err(IssueError::NotFound(_))
        ));
    }

    #[test]
    fn count_in_filters_by_status() {
        let store = temp_store();
        store.create("scout", "a", "x").unwrap();
        store.create("scout", "b", "x").unwrap();
        let c = store.create("scout", "c", "x").unwrap();
        store.set_status(&c.id, IssueStatus::Confirmed).unwrap();
        assert_eq!(store.count_in(IssueStatus::Pending).unwrap(), 2);
        assert_eq!(store.count_in(IssueStatus::Confirmed).unwrap(), 1);
    }
}
