//! Durable pending-timer journal.

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::AgentId;

const SCHEMA_VERSION: i64 = 1;
const APPLICATION_ID: i64 = 0x4c_48_54_4d;

#[derive(Debug, thiserror::Error)]
pub enum TimerJournalError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("timer journal: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeoutKind {
    Timeout,
    SelfReminder,
}

#[derive(Debug, Clone)]
pub struct TimeoutRequest {
    pub agent_id: AgentId,
    pub key: Option<String>,
    pub task_id: Option<String>,
    pub peer_id: Option<AgentId>,
    pub body: String,
    pub kind: TimeoutKind,
    pub fire_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct JournaledTimer {
    pub id: i64,
    pub request: TimeoutRequest,
}

fn connect(path: &Path) -> Result<Connection, TimerJournalError> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(connection)
}

fn initialize(path: &Path) -> Result<(), TimerJournalError> {
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
        return Err(TimerJournalError::Sqlite(rusqlite::Error::InvalidQuery));
    }
    if version == 0 {
        connection.execute_batch(&format!(
            "PRAGMA journal_mode = WAL;
             PRAGMA application_id = {APPLICATION_ID};
             BEGIN IMMEDIATE;
             CREATE TABLE pending_timers (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 agent_id TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 key TEXT,
                 peer_id TEXT,
                 task_id TEXT,
                 body TEXT NOT NULL,
                 fire_at_ms INTEGER NOT NULL
             );
             CREATE INDEX pending_timers_due ON pending_timers(fire_at_ms, id);
             PRAGMA user_version = {SCHEMA_VERSION};
             COMMIT;"
        ))?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct TimeoutJournal {
    path: PathBuf,
}

impl TimeoutJournal {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, TimerJournalError> {
        let path = path.into();
        initialize(&path)?;
        Ok(Self { path })
    }

    pub fn schedule(&self, request: &TimeoutRequest) -> Result<i64, TimerJournalError> {
        let connection = connect(&self.path)?;
        let key = request.key.as_deref();
        let peer_id = request.peer_id.as_ref().map(|peer| peer.as_str());
        let task_id = request.task_id.as_deref();
        connection.execute(
            "DELETE FROM pending_timers
             WHERE agent_id = ?1 AND kind = ?2 AND key IS ?3",
            rusqlite::params![request.agent_id.as_str(), kind_name(request.kind), key],
        )?;
        connection.execute(
            "INSERT INTO pending_timers(agent_id, kind, key, peer_id, task_id, body, fire_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                request.agent_id.as_str(),
                kind_name(request.kind),
                key,
                peer_id,
                task_id,
                request.body,
                request.fire_at_ms,
            ],
        )?;
        Ok(connection.last_insert_rowid())
    }

    pub fn restore(&self) -> Result<Vec<JournaledTimer>, TimerJournalError> {
        let connection = connect(&self.path)?;
        let mut statement = connection.prepare(
            "SELECT id, agent_id, kind, key, peer_id, task_id, body, fire_at_ms
             FROM pending_timers
             ORDER BY fire_at_ms, id",
        )?;
        let rows = statement.query_map([], |row| {
            let kind = row.get::<_, String>(2)?;
            Ok(JournaledTimer {
                id: row.get(0)?,
                request: TimeoutRequest {
                    agent_id: AgentId::new(row.get::<_, String>(1)?),
                    key: row.get(3)?,
                    peer_id: row.get::<_, Option<String>>(4)?.map(AgentId::new),
                    task_id: row.get(5)?,
                    body: row.get(6)?,
                    kind: parse_kind(&kind),
                    fire_at_ms: row.get(7)?,
                },
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(TimerJournalError::Sqlite)
    }

    pub fn ack(&self, id: i64) -> Result<bool, TimerJournalError> {
        let connection = connect(&self.path)?;
        let changed = connection.execute("DELETE FROM pending_timers WHERE id = ?1", [id])?;
        Ok(changed == 1)
    }

    pub fn cancel(&self, agent_id: &AgentId, key: &str) -> Result<bool, TimerJournalError> {
        let connection = connect(&self.path)?;
        let changed = connection.execute(
            "DELETE FROM pending_timers WHERE agent_id = ?1 AND key = ?2",
            rusqlite::params![agent_id.as_str(), key],
        )?;
        Ok(changed > 0)
    }

    pub fn pending_count(&self) -> Result<usize, TimerJournalError> {
        let connection = connect(&self.path)?;
        let count = connection.query_row("SELECT count(*) FROM pending_timers", [], |row| {
            row.get::<_, i64>(0)
        })?;
        Ok(usize::try_from(count).unwrap_or(usize::MAX))
    }
}

fn kind_name(kind: TimeoutKind) -> &'static str {
    match kind {
        TimeoutKind::Timeout => "timeout",
        TimeoutKind::SelfReminder => "self_reminder",
    }
}

fn parse_kind(value: &str) -> TimeoutKind {
    match value {
        "self_reminder" => TimeoutKind::SelfReminder,
        _ => TimeoutKind::Timeout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox::{InboxMessage, MessageType};
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn due_timer_restores_and_delivers_without_duplicate_row() {
        let temp = tempfile::tempdir().unwrap();
        let journal = TimeoutJournal::open(temp.path().join("timers.sqlite3")).unwrap();
        journal
            .schedule(&TimeoutRequest {
                agent_id: AgentId::new("alice"),
                key: Some("task".into()),
                task_id: Some("task".into()),
                peer_id: Some(AgentId::new("bob")),
                body: "follow up".into(),
                kind: TimeoutKind::Timeout,
                fire_at_ms: 0,
            })
            .unwrap();

        let context = crate::social_context::SocialContext::with_journal(
            10,
            None,
            Some(std::sync::Arc::new(journal.clone())),
        )
        .unwrap();
        let alice = AgentId::new("alice");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while !context.has_messages(&alice) {
            assert!(tokio::time::Instant::now() < deadline, "timer did not fire");
            tokio::time::advance(Duration::from_millis(10)).await;
        }
        assert_eq!(
            context
                .drain_inbox(&alice)
                .first()
                .map(|message| message.body.clone()),
            Some("follow up".to_string())
        );
        assert_eq!(journal.pending_count().unwrap(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn already_delivered_timer_is_deduplicated_on_restart() {
        let temp = tempfile::tempdir().unwrap();
        let timer_journal = TimeoutJournal::open(temp.path().join("timers.sqlite3")).unwrap();
        let inbox_journal =
            crate::inbox_journal::InboxJournal::open(temp.path().join("inbox.sqlite3")).unwrap();
        let timer_id = timer_journal
            .schedule(&TimeoutRequest {
                agent_id: AgentId::new("alice"),
                key: Some("task".into()),
                task_id: Some("task".into()),
                peer_id: Some(AgentId::new("bob")),
                body: "follow up".into(),
                kind: TimeoutKind::Timeout,
                fire_at_ms: 0,
            })
            .unwrap();
        inbox_journal
            .enqueue(&InboxMessage {
                journal_id: None,
                from: AgentId::new("timer"),
                to: AgentId::new("alice"),
                body: "follow up".into(),
                message_type: MessageType::Timeout,
                priority: crate::inbox::priority::TIMEOUT,
                task_id: Some("task".into()),
                broadcast_id: None,
            })
            .unwrap();

        let context = crate::social_context::SocialContext::with_journal(
            10,
            Some(Arc::new(inbox_journal)),
            Some(Arc::new(timer_journal.clone())),
        )
        .unwrap();
        let alice = AgentId::new("alice");

        assert!(context.has_messages(&alice));
        assert_eq!(context.drain_inbox(&alice).len(), 1);
        assert_eq!(timer_journal.pending_count().unwrap(), 0);
        assert_eq!(timer_id, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_timer_removes_journal_row() {
        let temp = tempfile::tempdir().unwrap();
        let journal = TimeoutJournal::open(temp.path().join("timers.sqlite3")).unwrap();
        let context = crate::social_context::SocialContext::with_journal(
            10,
            None,
            Some(std::sync::Arc::new(journal.clone())),
        )
        .unwrap();
        let alice = AgentId::new("alice");
        let timer = crate::timeout_manager::TimeoutManager::new(context.clone());
        timer.set_timeout(
            alice.clone(),
            "follow up",
            Duration::from_secs(60),
            Some("task".into()),
            Some(AgentId::new("bob")),
        );
        assert_eq!(journal.pending_count().unwrap(), 1);

        let reply = crate::inbox::InboxMessage {
            journal_id: None,
            from: AgentId::new("bob"),
            to: alice.clone(),
            body: "done".into(),
            message_type: crate::inbox::MessageType::Direct,
            priority: crate::inbox::priority::NORMAL,
            task_id: Some("task".into()),
            broadcast_id: None,
        };
        assert_eq!(
            context.cancel_timeouts_for_messages(&alice, &[reply]).len(),
            1
        );
        assert_eq!(journal.pending_count().unwrap(), 0);
    }

    fn request(kind: TimeoutKind, key: Option<String>) -> TimeoutRequest {
        TimeoutRequest {
            agent_id: AgentId::new("alice"),
            key: key.clone(),
            task_id: key,
            peer_id: None,
            body: "follow up".into(),
            kind,
            fire_at_ms: 100,
        }
    }

    #[test]
    fn schedule_restore_ack_and_cancel_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let journal = TimeoutJournal::open(temp.path().join("timers.sqlite3")).unwrap();
        let timeout_id = journal
            .schedule(&request(TimeoutKind::Timeout, Some("task".into())))
            .unwrap();
        let reminder_id = journal
            .schedule(&request(
                TimeoutKind::SelfReminder,
                Some("reminder:task".into()),
            ))
            .unwrap();

        assert_eq!(journal.restore().unwrap().len(), 2);
        assert_eq!(journal.pending_count().unwrap(), 2);
        assert!(journal.ack(timeout_id).unwrap());
        assert!(
            journal
                .cancel(&AgentId::new("alice"), "reminder:task")
                .unwrap()
        );
        assert!(!journal.ack(reminder_id).unwrap());
        assert_eq!(journal.pending_count().unwrap(), 0);
    }
}
