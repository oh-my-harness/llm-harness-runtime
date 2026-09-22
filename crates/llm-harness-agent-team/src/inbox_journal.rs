//! Durable pending-message journal for [`crate::inbox::InboxStore`].
//!
//! A row is inserted before the in-memory queue accepts a message and is
//! removed only after a runner reports successful consumption. A crash in any
//! intermediate state therefore redelivers the message on the next startup.

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::AgentId;

use crate::inbox::InboxMessage;

const SCHEMA_VERSION: i64 = 1;
const APPLICATION_ID: i64 = 0x4c_48_41_49;

#[derive(Debug, thiserror::Error)]
pub enum InboxJournalError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("inbox journal: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("inbox journal payload: {0}")]
    Payload(#[from] serde_json::Error),
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PendingMessage {
    pub(crate) id: i64,
    pub(crate) message: InboxMessage,
}

fn connect(path: &Path) -> Result<Connection, InboxJournalError> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(connection)
}

fn initialize(path: &Path) -> Result<(), InboxJournalError> {
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
        return Err(InboxJournalError::Sqlite(rusqlite::Error::InvalidQuery));
    }
    if version == 0 {
        connection.execute_batch(&format!(
            "PRAGMA journal_mode = WAL;
             PRAGMA application_id = {APPLICATION_ID};
             BEGIN IMMEDIATE;
             CREATE TABLE pending_messages (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 payload TEXT NOT NULL
             );
             PRAGMA user_version = {SCHEMA_VERSION};
             COMMIT;"
        ))?;
    }
    Ok(())
}

#[derive(Debug)]
pub struct InboxJournal {
    path: PathBuf,
}

impl InboxJournal {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, InboxJournalError> {
        let path = path.into();
        initialize(&path)?;
        Ok(Self { path })
    }

    pub fn enqueue(&self, message: &InboxMessage) -> Result<i64, InboxJournalError> {
        let payload = serde_json::to_string(message)?;
        let connection = connect(&self.path)?;
        connection.execute(
            "INSERT INTO pending_messages(payload) VALUES (?1)",
            [payload],
        )?;
        Ok(connection.last_insert_rowid())
    }

    pub(crate) fn has_timer_notification(
        &self,
        recipient: &AgentId,
        task_id: Option<&str>,
        body: &str,
        message_type: crate::inbox::MessageType,
    ) -> Result<bool, InboxJournalError> {
        Ok(self.recover()?.into_iter().any(|pending| {
            let message = pending.message;
            &message.to == recipient
                && message.body == body
                && message.message_type == message_type
                && message.task_id.as_deref() == task_id
        }))
    }

    pub(crate) fn recover(&self) -> Result<Vec<PendingMessage>, InboxJournalError> {
        let connection = connect(&self.path)?;
        let mut statement =
            connection.prepare("SELECT id, payload FROM pending_messages ORDER BY id")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (id, payload) = row?;
            Ok(PendingMessage {
                id,
                message: serde_json::from_str(&payload)?,
            })
        })
        .collect()
    }

    /// Number of messages that would be recovered on the next store build.
    pub fn pending_count(&self) -> Result<usize, InboxJournalError> {
        let connection = connect(&self.path)?;
        let count = connection.query_row("SELECT count(*) FROM pending_messages", [], |row| {
            row.get::<_, i64>(0)
        })?;
        Ok(usize::try_from(count).unwrap_or(usize::MAX))
    }

    pub fn ack(&self, id: i64) -> Result<bool, InboxJournalError> {
        let connection = connect(&self.path)?;
        let changed = connection.execute("DELETE FROM pending_messages WHERE id = ?1", [id])?;
        Ok(changed == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentId;
    use crate::inbox::{MessageType, priority};

    fn message(body: &str) -> InboxMessage {
        InboxMessage {
            journal_id: None,
            from: AgentId::new("a"),
            to: AgentId::new("b"),
            body: body.into(),
            message_type: MessageType::Direct,
            priority: priority::NORMAL,
            task_id: None,
            broadcast_id: None,
            message_id: None,
        }
    }

    #[test]
    fn enqueue_recover_and_ack_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("inbox.sqlite3");
        let journal = InboxJournal::open(&path).unwrap();
        let first_id = journal.enqueue(&message("first")).unwrap();
        let second_id = journal.enqueue(&message("second")).unwrap();

        let recovered = journal.recover().unwrap();
        assert_eq!(recovered.len(), 2);
        assert_eq!(recovered[0].id, first_id);
        assert_eq!(recovered[1].id, second_id);
        assert_eq!(recovered[0].message.body, "first");

        assert!(journal.ack(first_id).unwrap());
        assert!(!journal.ack(first_id).unwrap());
        let recovered = journal.recover().unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].message.body, "second");
    }

    #[test]
    fn stable_message_id_round_trips_for_restart_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("inbox.sqlite3");
        let journal = InboxJournal::open(&path).unwrap();
        let mut sent = message("durable message");
        sent.message_id = Some("stable-message-id".to_string());
        let row_id = journal.enqueue(&sent).unwrap();

        let recovered = journal.recover().unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].id, row_id);
        assert_eq!(
            recovered[0].message.message_id.as_deref(),
            Some("stable-message-id")
        );
    }

    #[test]
    fn legacy_broadcast_recipients_recover_one_logical_message_id() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("inbox.sqlite3");
        let journal = InboxJournal::open(&path).unwrap();
        let mut first = message("group message");
        first.message_type = MessageType::Broadcast;
        first.broadcast_id = Some("legacy-broadcast".to_string());
        let mut second = first.clone();
        second.to = AgentId::new("c");
        journal.enqueue(&first).unwrap();
        journal.enqueue(&second).unwrap();

        let recovered = journal.recover().unwrap();
        assert_eq!(recovered.len(), 2);
        assert_eq!(
            recovered[0].message.message_id,
            recovered[1].message.message_id
        );
    }
}
