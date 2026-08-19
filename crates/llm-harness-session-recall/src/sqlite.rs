use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use llm_harness_types::EntryId;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{
    SessionRecallCandidate, SessionRecallDocument, SessionRecallError, SessionRecallIndex,
    SessionRecallIndexQuery, SessionRecallProjection, SessionRecallRef, truncate_utf8,
};

const APPLICATION_ID: i64 = 0x4c48_5252;
const SCHEMA_VERSION: i64 = 1;

/// Persistent SQLite FTS5 projection for local Session Recall deployments.
///
/// Every operation opens a short-lived connection on a blocking worker.
/// Writes are serialized and transactional; searches may run concurrently.
/// Session bodies remain authoritative in `SessionRepo` and are revalidated by
/// `SessionRecallService` before any candidate is emitted.
pub struct SqliteSessionRecallIndex {
    path: Arc<PathBuf>,
    write_lock: Arc<Mutex<()>>,
}

impl SqliteSessionRecallIndex {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SessionRecallError> {
        let path = path.into();
        initialize(&path)?;
        Ok(Self {
            path: Arc::new(path),
            write_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn path(&self) -> &Path {
        self.path.as_ref()
    }
}

impl SessionRecallIndex for SqliteSessionRecallIndex {
    fn search(
        &self,
        request: SessionRecallIndexQuery,
        abort: CancellationToken,
    ) -> BoxFuture<'_, Result<Vec<SessionRecallCandidate>, SessionRecallError>> {
        let path = self.path.clone();
        Box::pin(async move {
            check_abort(&abort)?;
            if request.limit == 0 || request.max_snippet_bytes == 0 {
                return Ok(vec![]);
            }
            let query = fts_query(&request.query);
            if query.is_empty() {
                return Ok(vec![]);
            }
            let scope = serde_json::to_string(&request.scope)
                .map_err(|_| SessionRecallError::ProjectionFailed)?;
            let rows = tokio::task::spawn_blocking(move || {
                let connection = connect(&path)?;
                let mut statement = connection
                    .prepare(
                        "SELECT d.session_id, d.user_entry_id, d.assistant_entry_id, \
                                d.branch_leaf_id, d.revision, d.title, d.user_text, \
                                d.assistant_text, d.updated_at, bm25(documents_fts) AS rank \
                         FROM documents_fts \
                         JOIN documents d ON d.rowid = documents_fts.rowid \
                         JOIN sessions s ON s.session_id = d.session_id \
                         WHERE documents_fts MATCH ?1 AND s.scope_json = ?2 \
                           AND (?3 IS NULL OR d.session_id <> ?3) \
                         ORDER BY rank ASC, d.updated_at DESC, d.session_id ASC, d.rowid ASC \
                         LIMIT ?4",
                    )
                    .map_err(map_sqlite_error)?;
                let limit = to_i64(request.limit as u64)?;
                let mapped = statement
                    .query_map(
                        params![query, scope, request.exclude_session_id, limit],
                        |row| {
                            Ok(StoredDocument {
                                session_id: row.get(0)?,
                                user_entry_id: row.get(1)?,
                                assistant_entry_id: row.get(2)?,
                                branch_leaf_id: row.get(3)?,
                                revision: row.get(4)?,
                                title: row.get(5)?,
                                user_text: row.get(6)?,
                                assistant_text: row.get(7)?,
                                updated_at: row.get(8)?,
                                rank: row.get(9)?,
                            })
                        },
                    )
                    .map_err(map_sqlite_error)?;
                mapped
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(map_sqlite_error)
            })
            .await
            .map_err(|_| SessionRecallError::ProjectionFailed)??;
            check_abort(&abort)?;
            rows.into_iter()
                .map(|row| row.into_candidate(request.max_snippet_bytes))
                .collect()
        })
    }

    fn replace_session(
        &self,
        projection: SessionRecallProjection,
    ) -> BoxFuture<'_, Result<bool, SessionRecallError>> {
        let path = self.path.clone();
        let write_lock = self.write_lock.clone();
        Box::pin(async move {
            let _guard = write_lock.lock().await;
            tokio::task::spawn_blocking(move || {
                let mut connection = connect(&path)?;
                let transaction = connection.transaction().map_err(map_sqlite_error)?;
                let revision = to_i64(projection.revision)?;
                let tombstone = transaction
                    .query_row(
                        "SELECT revision, permanent FROM tombstones WHERE session_id = ?1",
                        [&projection.session_id],
                        |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, bool>(1)?)),
                    )
                    .optional()
                    .map_err(map_sqlite_error)?;
                if tombstone.is_some_and(|(tombstone_revision, permanent)| {
                    permanent || tombstone_revision.is_some_and(|stored| stored >= revision)
                }) {
                    return Ok(false);
                }
                let stored_revision = transaction
                    .query_row(
                        "SELECT revision FROM sessions WHERE session_id = ?1",
                        [&projection.session_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()
                    .map_err(map_sqlite_error)?;
                if stored_revision.is_some_and(|stored| stored >= revision) {
                    return Ok(false);
                }
                transaction
                    .execute(
                        "DELETE FROM sessions WHERE session_id = ?1",
                        [&projection.session_id],
                    )
                    .map_err(map_sqlite_error)?;
                transaction
                    .execute(
                        "DELETE FROM tombstones WHERE session_id = ?1",
                        [&projection.session_id],
                    )
                    .map_err(map_sqlite_error)?;
                insert_projection(&transaction, &projection)?;
                transaction.commit().map_err(map_sqlite_error)?;
                Ok(true)
            })
            .await
            .map_err(|_| SessionRecallError::ProjectionFailed)?
        })
    }

    fn remove_session(
        &self,
        session_id: String,
        revision: u64,
        permanent: bool,
    ) -> BoxFuture<'_, Result<bool, SessionRecallError>> {
        let path = self.path.clone();
        let write_lock = self.write_lock.clone();
        Box::pin(async move {
            let _guard = write_lock.lock().await;
            tokio::task::spawn_blocking(move || {
                let mut connection = connect(&path)?;
                let transaction = connection.transaction().map_err(map_sqlite_error)?;
                let revision = to_i64(revision)?;
                let stored_revision = transaction
                    .query_row(
                        "SELECT revision FROM sessions WHERE session_id = ?1",
                        [&session_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()
                    .map_err(map_sqlite_error)?;
                if !permanent && stored_revision.is_some_and(|stored| stored > revision) {
                    return Ok(false);
                }
                transaction
                    .execute("DELETE FROM sessions WHERE session_id = ?1", [&session_id])
                    .map_err(map_sqlite_error)?;
                let existing = transaction
                    .query_row(
                        "SELECT revision, permanent FROM tombstones WHERE session_id = ?1",
                        [&session_id],
                        |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, bool>(1)?)),
                    )
                    .optional()
                    .map_err(map_sqlite_error)?;
                let final_permanent = permanent || existing.is_some_and(|(_, value)| value);
                let final_revision = if final_permanent {
                    None
                } else {
                    Some(
                        existing
                            .and_then(|(revision, _)| revision)
                            .unwrap_or_default()
                            .max(revision),
                    )
                };
                transaction
                    .execute(
                        "INSERT INTO tombstones(session_id, revision, permanent) VALUES(?1, ?2, ?3) \
                         ON CONFLICT(session_id) DO UPDATE SET revision = excluded.revision, \
                         permanent = excluded.permanent",
                        params![session_id, final_revision, final_permanent],
                    )
                    .map_err(map_sqlite_error)?;
                transaction.commit().map_err(map_sqlite_error)?;
                Ok(true)
            })
            .await
            .map_err(|_| SessionRecallError::ProjectionFailed)?
        })
    }

    fn projected_revisions(
        &self,
    ) -> BoxFuture<'_, Result<BTreeMap<String, u64>, SessionRecallError>> {
        let path = self.path.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let connection = connect(&path)?;
                let mut statement = connection
                    .prepare("SELECT session_id, revision FROM sessions ORDER BY session_id")
                    .map_err(map_sqlite_error)?;
                let rows = statement
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                    })
                    .map_err(map_sqlite_error)?;
                let mut revisions = BTreeMap::new();
                for row in rows {
                    let (session_id, revision) = row.map_err(map_sqlite_error)?;
                    revisions.insert(
                        session_id,
                        u64::try_from(revision)
                            .map_err(|_| SessionRecallError::ProjectionFailed)?,
                    );
                }
                Ok(revisions)
            })
            .await
            .map_err(|_| SessionRecallError::ProjectionFailed)?
        })
    }

    fn replace_all(
        &self,
        projections: Vec<SessionRecallProjection>,
    ) -> BoxFuture<'_, Result<(), SessionRecallError>> {
        let path = self.path.clone();
        let write_lock = self.write_lock.clone();
        Box::pin(async move {
            let _guard = write_lock.lock().await;
            tokio::task::spawn_blocking(move || {
                let mut latest: BTreeMap<String, SessionRecallProjection> = BTreeMap::new();
                for projection in projections {
                    match latest.get(&projection.session_id) {
                        Some(current) if current.revision > projection.revision => {}
                        _ => {
                            latest.insert(projection.session_id.clone(), projection);
                        }
                    }
                }
                let mut connection = connect(&path)?;
                let transaction = connection.transaction().map_err(map_sqlite_error)?;
                transaction
                    .execute("DELETE FROM sessions", [])
                    .map_err(map_sqlite_error)?;
                transaction
                    .execute("DELETE FROM tombstones", [])
                    .map_err(map_sqlite_error)?;
                for projection in latest.values() {
                    insert_projection(&transaction, projection)?;
                }
                transaction.commit().map_err(map_sqlite_error)
            })
            .await
            .map_err(|_| SessionRecallError::ProjectionFailed)?
        })
    }

    fn reset(&self) -> BoxFuture<'_, Result<(), SessionRecallError>> {
        let path = self.path.clone();
        let write_lock = self.write_lock.clone();
        Box::pin(async move {
            let _guard = write_lock.lock().await;
            tokio::task::spawn_blocking(move || {
                let mut connection = connect(&path)?;
                let transaction = connection.transaction().map_err(map_sqlite_error)?;
                transaction
                    .execute("DELETE FROM sessions", [])
                    .map_err(map_sqlite_error)?;
                transaction
                    .execute("DELETE FROM tombstones", [])
                    .map_err(map_sqlite_error)?;
                transaction.commit().map_err(map_sqlite_error)
            })
            .await
            .map_err(|_| SessionRecallError::ProjectionFailed)?
        })
    }
}

struct StoredDocument {
    session_id: String,
    user_entry_id: String,
    assistant_entry_id: Option<String>,
    branch_leaf_id: String,
    revision: i64,
    title: Option<String>,
    user_text: String,
    assistant_text: Option<String>,
    updated_at: String,
    rank: f64,
}

impl StoredDocument {
    fn into_candidate(
        self,
        max_snippet_bytes: usize,
    ) -> Result<SessionRecallCandidate, SessionRecallError> {
        let document = SessionRecallDocument {
            reference: SessionRecallRef {
                session_id: self.session_id,
                user_entry_id: EntryId::from_str(&self.user_entry_id)
                    .map_err(|_| SessionRecallError::ProjectionFailed)?,
                assistant_entry_id: self
                    .assistant_entry_id
                    .map(|entry_id| EntryId::from_str(&entry_id))
                    .transpose()
                    .map_err(|_| SessionRecallError::ProjectionFailed)?,
                branch_leaf_id: EntryId::from_str(&self.branch_leaf_id)
                    .map_err(|_| SessionRecallError::ProjectionFailed)?,
                revision: u64::try_from(self.revision)
                    .map_err(|_| SessionRecallError::ProjectionFailed)?,
            },
            title: self.title,
            user_text: self.user_text,
            assistant_text: self.assistant_text,
            updated_at: DateTime::parse_from_rfc3339(&self.updated_at)
                .map_err(|_| SessionRecallError::ProjectionFailed)?
                .with_timezone(&Utc),
        };
        let (snippet, _) = truncate_utf8(&document.formatted_content(), max_snippet_bytes);
        Ok(SessionRecallCandidate {
            reference: document.reference,
            title: document.title,
            snippet,
            score: (-self.rank) as f32,
            updated_at: document.updated_at,
        })
    }
}

fn initialize(path: &Path) -> Result<(), SessionRecallError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|_| SessionRecallError::ProjectionFailed)?;
    }
    let mut connection = connect(path)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite_error)?;
    let application_id = transaction
        .query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0))
        .map_err(map_sqlite_error)?;
    let version = transaction
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(map_sqlite_error)?;
    if (application_id != 0 && application_id != APPLICATION_ID)
        || version > SCHEMA_VERSION
        || (version > 0 && application_id != APPLICATION_ID)
    {
        return Err(SessionRecallError::ProjectionFailed);
    }
    if version == 0 {
        transaction
            .execute_batch(&format!(
                "PRAGMA application_id = {APPLICATION_ID};
                 CREATE TABLE sessions (
                     session_id TEXT PRIMARY KEY,
                     revision INTEGER NOT NULL,
                     scope_json TEXT NOT NULL
                 );
                 CREATE TABLE tombstones (
                     session_id TEXT PRIMARY KEY,
                     revision INTEGER,
                     permanent INTEGER NOT NULL CHECK(permanent IN (0, 1))
                 );
                 CREATE TABLE documents (
                     rowid INTEGER PRIMARY KEY,
                     session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
                     user_entry_id TEXT NOT NULL,
                     assistant_entry_id TEXT,
                     branch_leaf_id TEXT NOT NULL,
                     revision INTEGER NOT NULL,
                     title TEXT,
                     user_text TEXT NOT NULL,
                     assistant_text TEXT,
                     updated_at TEXT NOT NULL
                 );
                 CREATE INDEX documents_session_id ON documents(session_id);
                 CREATE VIRTUAL TABLE documents_fts USING fts5(
                     title, user_text, assistant_text,
                     content='documents', content_rowid='rowid', tokenize='trigram'
                 );
                 CREATE TRIGGER documents_ai AFTER INSERT ON documents BEGIN
                     INSERT INTO documents_fts(rowid, title, user_text, assistant_text)
                     VALUES (new.rowid, new.title, new.user_text, new.assistant_text);
                 END;
                 CREATE TRIGGER documents_ad AFTER DELETE ON documents BEGIN
                     INSERT INTO documents_fts(documents_fts, rowid, title, user_text, assistant_text)
                     VALUES ('delete', old.rowid, old.title, old.user_text, old.assistant_text);
                 END;
                 PRAGMA user_version = {SCHEMA_VERSION};"
            ))
            .map_err(map_sqlite_error)?;
    }
    transaction.commit().map_err(map_sqlite_error)?;
    for attempt in 0..=100 {
        match connection.query_row("PRAGMA journal_mode = WAL", [], |row| {
            row.get::<_, String>(0)
        }) {
            Ok(_) => return Ok(()),
            Err(error) if is_locked(&error) && attempt < 100 => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(map_sqlite_error(error)),
        }
    }
    Err(SessionRecallError::ProjectionFailed)
}

fn connect(path: &Path) -> Result<Connection, SessionRecallError> {
    let connection = Connection::open(path).map_err(map_sqlite_error)?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(map_sqlite_error)?;
    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(map_sqlite_error)?;
    Ok(connection)
}

fn insert_projection(
    transaction: &Transaction<'_>,
    projection: &SessionRecallProjection,
) -> Result<(), SessionRecallError> {
    let revision = to_i64(projection.revision)?;
    let scope = serde_json::to_string(&projection.scope)
        .map_err(|_| SessionRecallError::ProjectionFailed)?;
    transaction
        .execute(
            "INSERT INTO sessions(session_id, revision, scope_json) VALUES(?1, ?2, ?3)",
            params![&projection.session_id, revision, scope],
        )
        .map_err(map_sqlite_error)?;
    let mut statement = transaction
        .prepare(
            "INSERT INTO documents(
                session_id, user_entry_id, assistant_entry_id, branch_leaf_id,
                revision, title, user_text, assistant_text, updated_at
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )
        .map_err(map_sqlite_error)?;
    for document in &projection.documents {
        statement
            .execute(params![
                &projection.session_id,
                document.reference.user_entry_id.to_string(),
                document
                    .reference
                    .assistant_entry_id
                    .map(|entry_id| entry_id.to_string()),
                document.reference.branch_leaf_id.to_string(),
                revision,
                document.title.as_deref(),
                &document.user_text,
                document.assistant_text.as_deref(),
                document.updated_at.to_rfc3339(),
            ])
            .map_err(map_sqlite_error)?;
    }
    Ok(())
}

fn fts_query(query: &str) -> String {
    super::index::terms(query)
        .into_iter()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn to_i64(value: u64) -> Result<i64, SessionRecallError> {
    i64::try_from(value).map_err(|_| SessionRecallError::ProjectionFailed)
}

fn check_abort(abort: &CancellationToken) -> Result<(), SessionRecallError> {
    if abort.is_cancelled() {
        Err(SessionRecallError::Aborted)
    } else {
        Ok(())
    }
}

fn map_sqlite_error(_: rusqlite::Error) -> SessionRecallError {
    SessionRecallError::ProjectionFailed
}

fn is_locked(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}
