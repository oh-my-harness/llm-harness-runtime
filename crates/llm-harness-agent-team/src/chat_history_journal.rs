//! Durable team chat transcript.
//!
//! The pending-message journal answers "what work still needs delivery?"; this
//! journal answers "what did the team say?". A row is inserted when a social
//! message first enters the system and is never deleted by delivery. The
//! logical message ID is unique so one broadcast (which fans out into
//! recipient-specific inbox rows) appears as one transcript message.

use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::inbox::InboxMessage;

const SCHEMA_VERSION: i64 = 1;
const APPLICATION_ID: i64 = 0x4c_48_43_54;

#[derive(Debug, thiserror::Error)]
pub enum ChatHistoryJournalError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("chat history journal: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("chat history payload: {0}")]
    Payload(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatHistoryEntry {
    /// Monotonic sequence assigned by durable storage. `None` only for
    /// teams that intentionally run without a durable transcript.
    pub sequence: Option<i64>,
    pub created_at: Option<String>,
    pub message: InboxMessage,
}

fn connect(path: &Path) -> Result<Connection, ChatHistoryJournalError> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(connection)
}

fn initialize(path: &Path) -> Result<(), ChatHistoryJournalError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let connection = connect(path)?;
    let application_id: i64 =
        connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if (application_id != 0 && application_id != APPLICATION_ID)
        || version > SCHEMA_VERSION
        || (version > 0 && application_id != APPLICATION_ID)
    {
        return Err(ChatHistoryJournalError::Sqlite(
            rusqlite::Error::InvalidQuery,
        ));
    }
    if version == 0 {
        connection.execute_batch(&format!(
            "PRAGMA journal_mode = WAL;
             PRAGMA application_id = {APPLICATION_ID};
             BEGIN IMMEDIATE;
             CREATE TABLE chat_messages (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 message_id TEXT NOT NULL UNIQUE,
                 from_id TEXT NOT NULL,
                 to_id TEXT NOT NULL,
                 message_type TEXT NOT NULL,
                 broadcast_id TEXT,
                 created_at TEXT NOT NULL,
                 payload TEXT NOT NULL
             );
             CREATE INDEX idx_chat_messages_group
                 ON chat_messages(message_type, to_id, broadcast_id);
             CREATE INDEX idx_chat_messages_private
                 ON chat_messages(from_id, to_id, broadcast_id);
             PRAGMA user_version = {SCHEMA_VERSION};
             COMMIT;"
        ))?;
    }
    Ok(())
}

#[derive(Debug)]
pub struct ChatHistoryJournal {
    path: PathBuf,
}

impl ChatHistoryJournal {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, ChatHistoryJournalError> {
        let path = path.into();
        initialize(&path)?;
        Ok(Self { path })
    }

    pub fn append(&self, message: &InboxMessage) -> Result<Option<i64>, ChatHistoryJournalError> {
        let payload = serde_json::to_string(message)?;
        let created_at = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let connection = connect(&self.path)?;
        let changed = connection.execute(
            "INSERT OR IGNORE INTO chat_messages
                (message_id, from_id, to_id, message_type, broadcast_id, created_at, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                message.message_id.as_deref().ok_or_else(|| {
                    rusqlite::Error::InvalidParameterName("message_id is required".to_string())
                })?,
                message.from.as_str(),
                message.to.as_str(),
                message_type_name(&message.message_type),
                message.broadcast_id.as_deref(),
                created_at,
                payload,
            ],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        Ok(Some(connection.last_insert_rowid()))
    }

    pub fn group_history(
        &self,
        limit: usize,
        before: Option<i64>,
    ) -> Result<Vec<ChatHistoryEntry>, ChatHistoryJournalError> {
        let sql = "SELECT id, payload, created_at FROM chat_messages
             WHERE (?1 IS NULL OR id < ?1)
                AND (
                    message_type = 'broadcast'
                    OR (
                        message_type = 'direct'
                        AND to_id = ?3
                    )
                )
             ORDER BY id DESC LIMIT ?2";
        self.query(
            sql,
            limit,
            before,
            false,
            Some(crate::timeout_manager::reserved::OPERATOR),
            None,
        )
    }

    pub fn private_history(
        &self,
        peer_a: &crate::AgentId,
        peer_b: &crate::AgentId,
        limit: usize,
        before: Option<i64>,
    ) -> Result<Vec<ChatHistoryEntry>, ChatHistoryJournalError> {
        let sql = "SELECT id, payload, created_at FROM chat_messages
             WHERE message_type = 'direct'
                AND broadcast_id IS NULL
                AND ((from_id = ?1 AND to_id = ?2) OR (from_id = ?3 AND to_id = ?4))
                AND (?5 IS NULL OR id < ?5)
             ORDER BY id DESC LIMIT ?6";
        self.query(
            sql,
            limit,
            before,
            true,
            Some(peer_a.as_str()),
            Some(peer_b.as_str()),
        )
    }

    fn query(
        &self,
        sql: &str,
        limit: usize,
        before: Option<i64>,
        private: bool,
        peer_a: Option<&str>,
        peer_b: Option<&str>,
    ) -> Result<Vec<ChatHistoryEntry>, ChatHistoryJournalError> {
        let connection = connect(&self.path)?;
        let mut statement = connection.prepare(sql)?;
        let mut query_rows = if private {
            statement.query(rusqlite::params![
                peer_a,
                peer_b,
                peer_b,
                peer_a,
                before,
                limit as i64
            ])
        } else {
            statement.query(rusqlite::params![before, limit as i64, peer_a])
        }?;
        let mut entries = Vec::new();
        while let Some(row) = query_rows.next()? {
            let id = row.get::<_, i64>(0)?;
            let payload = row.get::<_, String>(1)?;
            let created_at = row.get::<_, String>(2)?;
            let message: InboxMessage = serde_json::from_str(&payload)?;
            entries.push(ChatHistoryEntry {
                sequence: Some(id),
                created_at: Some(created_at),
                message,
            });
        }
        entries.reverse();
        Ok(entries)
    }
}

fn message_type_name(message_type: &crate::inbox::MessageType) -> &'static str {
    match message_type {
        crate::inbox::MessageType::Broadcast => "broadcast",
        crate::inbox::MessageType::Direct => "direct",
        crate::inbox::MessageType::DelegationRequest => "delegation_request",
        crate::inbox::MessageType::Timeout => "timeout",
        crate::inbox::MessageType::SelfReminder => "self_reminder",
        crate::inbox::MessageType::ReflectionTrigger => "reflection_trigger",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentId;
    use crate::inbox::MessageType;

    fn direct(
        from: &str,
        to: &str,
        body: &str,
        message_id: &str,
        broadcast_id: Option<&str>,
    ) -> InboxMessage {
        InboxMessage {
            journal_id: None,
            from: AgentId::new(from),
            to: AgentId::new(to),
            body: body.into(),
            message_type: MessageType::Direct,
            priority: crate::inbox::priority::NORMAL,
            task_id: None,
            broadcast_id: broadcast_id.map(str::to_string),
            message_id: Some(message_id.into()),
        }
    }

    fn broadcast(message_id: &str, broadcast_id: &str) -> InboxMessage {
        InboxMessage {
            journal_id: None,
            from: crate::AgentId::new("operator"),
            to: crate::AgentId::new("planner"),
            body: "group message".into(),
            message_type: MessageType::Broadcast,
            priority: 0,
            task_id: None,
            broadcast_id: Some(broadcast_id.to_string()),
            message_id: Some(message_id.into()),
        }
    }

    #[test]
    fn append_folds_broadcast_recipient_copies_by_logical_id() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("chat.sqlite3");
        let journal = ChatHistoryJournal::open(&path).unwrap();
        let mut first = broadcast("message-1", "broadcast-1");
        let mut second = first.clone();
        second.to = crate::AgentId::new("coder");
        first.message_id = Some("message-1".into());
        second.message_id = Some("message-1".into());

        assert!(journal.append(&first).unwrap().is_some());
        assert!(journal.append(&second).unwrap().is_none());

        let entries = journal.group_history(10, None).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message.body, "group message");
        assert_eq!(entries[0].sequence, Some(1));
        assert!(entries[0].created_at.is_some());
    }

    #[test]
    fn direct_messages_are_kept_out_of_group_history() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("chat.sqlite3");
        let journal = ChatHistoryJournal::open(&path).unwrap();
        let original = direct("operator", "planner", "private", "private-1", None);
        journal.append(&original).unwrap();

        let group = journal.group_history(10, None).unwrap();
        assert!(group.is_empty());

        let entries = journal
            .private_history(
                &crate::AgentId::new("operator"),
                &crate::AgentId::new("planner"),
                10,
                None,
            )
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message.body, "private");
        assert_eq!(entries[0].sequence, Some(1));
        assert!(entries[0].created_at.is_some());
    }

    #[test]
    fn member_reports_to_operator_are_group_visible_without_broadcast_id() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("chat.sqlite3");
        let journal = ChatHistoryJournal::open(&path).unwrap();

        let operator_task = direct("operator", "verifier", "private task", "message-1", None);
        let verifier_report = direct("verifier", "operator", "FAIL", "message-2", None);
        journal.append(&operator_task).unwrap();
        journal.append(&verifier_report).unwrap();

        let group = journal.group_history(10, None).unwrap();
        assert_eq!(group.len(), 1);
        assert_eq!(group[0].message.body, "FAIL");

        let private_entries = journal
            .private_history(
                &crate::AgentId::new("operator"),
                &crate::AgentId::new("verifier"),
                10,
                None,
            )
            .unwrap();
        assert_eq!(private_entries.len(), 2);
    }

    #[test]
    fn group_replies_are_group_visible_and_cursor_pages_older_rows() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("chat.sqlite3");
        let journal = ChatHistoryJournal::open(&path).unwrap();

        let prompt = direct(
            "operator",
            "planner",
            "group prompt",
            "message-1",
            Some("broadcast-1"),
        );
        let mut prompt = prompt;
        prompt.message_type = MessageType::Broadcast;
        journal.append(&prompt).unwrap();
        let reply = direct(
            "planner",
            "operator",
            "group reply",
            "message-2",
            Some("broadcast-1"),
        );
        journal.append(&reply).unwrap();
        let private = direct("operator", "planner", "private note", "message-3", None);
        journal.append(&private).unwrap();

        let group = journal.group_history(10, None).unwrap();
        assert_eq!(group.len(), 2);
        assert_eq!(group[0].message.body, "group prompt");
        assert_eq!(group[1].message.body, "group reply");

        let paged = journal
            .group_history(1, Some(group[1].sequence.unwrap()))
            .unwrap();
        assert_eq!(paged.len(), 1);
        assert_eq!(paged[0].message.body, "group prompt");

        let private_entries = journal
            .private_history(
                &crate::AgentId::new("operator"),
                &crate::AgentId::new("planner"),
                10,
                None,
            )
            .unwrap();
        assert_eq!(private_entries.len(), 1);
        assert_eq!(private_entries[0].message.body, "private note");
    }

    #[test]
    fn group_cursor_bounds_broadcasts_and_replies() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("chat.sqlite3");
        let journal = ChatHistoryJournal::open(&path).unwrap();

        let older_prompt = direct(
            "operator",
            "planner",
            "older prompt",
            "message-1",
            Some("broadcast-1"),
        );
        journal
            .append(&InboxMessage {
                message_type: MessageType::Broadcast,
                ..older_prompt
            })
            .unwrap();
        let newer_reply = direct(
            "planner",
            "operator",
            "newer reply",
            "message-2",
            Some("broadcast-1"),
        );
        journal.append(&newer_reply).unwrap();

        let paged = journal.group_history(1, Some(2)).unwrap();
        assert_eq!(paged.len(), 1);
        assert_eq!(paged[0].message.body, "older prompt");
    }
}
