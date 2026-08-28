//! SQLite-backed persistent memory store for agent teams.
//!
//! Backs the `memory_write` / `memory_forget` tools with a durable SQLite
//! database (FTS5 full-text search), so lessons recorded by the reflection
//! loop survive process restarts and are retrievable via `knowledge_search`
//! by the agents that wrote them.
//!
//! Two halves share one database file (pattern ported from Folumi's Saved
//! Memory bridge, trimmed of product-specific approval/kind whitelisting):
//!
//! - [`TeamSqliteMemorySource`]: the read side — a `KnowledgeSource`
//!   advertising `Search`/`Read`/`Revisioned`, backed by FTS5 recall and
//!   exact-reference reads with revision validation.
//! - [`TeamSqliteMemoryStore`]: the write side — a `MemoryStore` whose
//!   `upsert`/`delete` write the same database and return receipts whose
//!   `KnowledgeRef` matches what the read side hands out (both halves share
//!   one source id, so references round-trip).
//!
//! `MemoryService` requires the write store's `read_source_id` to match a
//! read source advertising `Search` + `Read` (+ `Revisioned` for revision
//! validation) — using the same `TeamSqliteMemorySource` for both halves
//! closes the reflection loop: a lesson written via `memory_write` is
//! immediately findable by the next `knowledge_search`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use llm_harness_knowledge::{
    ContentSelector, FreshnessClass, KnowledgeAccessContext, KnowledgeCapability, KnowledgeContent,
    KnowledgeError, KnowledgeHit, KnowledgeReadRequest, KnowledgeRef, KnowledgeRequestContext,
    KnowledgeScope, KnowledgeSource, KnowledgeSourceDescriptor, PrincipalRef, SourceSearchPage,
    SourceSearchRequest,
};
use llm_harness_memory::{
    MemoryConsistency, MemoryDeleteReceipt, MemoryStore as RuntimeMemoryStore,
    MemoryStoreDescriptor, MemoryVisibility, MemoryWrite, MemoryWriteReceipt,
};
use llm_harness_types::content::DataBlock;
use rusqlite::{Connection, OptionalExtension};
use tokio_util::sync::CancellationToken;

/// Source id for the team's persistent memory. Every `KnowledgeRef` handed
/// out by `memory_write`, `knowledge_search`, and `knowledge_read` over this
/// backend carries this id.
pub const TEAM_MEMORY_SOURCE_ID: &str = "agent-team.memory";

/// Bump when the schema below changes in a backward-incompatible way.
const SCHEMA_VERSION: i64 = 1;
/// `PRAGMA application_id` guard ("LHAM" — LLM Harness Agent-team Memory).
const APPLICATION_ID: i64 = 0x4C_48_41_4D;

/// `knowledge_search` hits return this placeholder snippet instead of the
/// content itself; the agent follows up with `knowledge_read` using the
/// exact reference. Keeps search payloads small and forces revision-aware
/// reads.
const TEAM_MEMORY_SNIPPET: &str = "Team memory match. Use knowledge_read with the exact reference and revision to read its content.";

fn db_error(e: rusqlite::Error) -> KnowledgeError {
    KnowledgeError::Backend(format!("team memory db: {e}"))
}

/// Open a short-lived connection with the pragmas every operation needs.
/// Each operation opens its own connection (same pattern as
/// `llm-harness-session-recall`'s sqlite index): SQLite's own locking plus a
/// busy timeout serialize concurrent runners; WAL keeps readers unblocked
/// while a write is in flight.
fn connect(path: &Path) -> Result<Connection, KnowledgeError> {
    let connection = Connection::open(path).map_err(db_error)?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(db_error)?;
    Ok(connection)
}

/// Create the database file and schema if absent; guard against opening a
/// foreign database (mismatched application id) or a newer schema.
fn initialize(path: &Path) -> Result<(), KnowledgeError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| KnowledgeError::Backend(format!("create memory db dir: {e}")))?;
    }
    let connection = connect(path)?;
    let application_id: i64 = connection
        .query_row("PRAGMA application_id", [], |row| row.get(0))
        .map_err(db_error)?;
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(db_error)?;
    if (application_id != 0 && application_id != APPLICATION_ID)
        || version > SCHEMA_VERSION
        || (version > 0 && application_id != APPLICATION_ID)
    {
        return Err(KnowledgeError::Backend(format!(
            "{path:?} is not a team memory database (application_id={application_id}, user_version={version})"
        )));
    }
    if version == 0 {
        connection
            .execute_batch(&format!(
                "PRAGMA journal_mode = WAL;
                 PRAGMA application_id = {APPLICATION_ID};
                 BEGIN IMMEDIATE;
                 CREATE TABLE memory_items (
                     item_id TEXT PRIMARY KEY,
                     revision TEXT NOT NULL,
                     content TEXT NOT NULL,
                     kind TEXT,
                     provenance_json TEXT NOT NULL DEFAULT '{{}}',
                     expires_at TEXT,
                     created_at TEXT NOT NULL
                 );
                 CREATE VIRTUAL TABLE memory_items_fts USING fts5(
                     item_id UNINDEXED,
                     content,
                     tokenize = 'unicode61'
                 );
                 PRAGMA user_version = {SCHEMA_VERSION};
                 COMMIT;"
            ))
            .map_err(db_error)?;
    }
    Ok(())
}

/// Item id derived from the idempotency key. `SecureMemoryWritePolicy`
/// already derives the key as a keyed hash of (principal, provenance,
/// content, kind, ttl), so an identical re-write converges to the same item
/// id and the `ON CONFLICT` path updates the existing row in place.
fn item_id_for(idempotency_key: &str) -> String {
    format!("mem-{idempotency_key}")
}

fn parse_created_at(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

fn team_reference(item_id: &str, revision: &str) -> KnowledgeRef {
    KnowledgeRef {
        source_id: TEAM_MEMORY_SOURCE_ID.to_string(),
        item_id: item_id.to_string(),
        revision: Some(revision.to_string()),
    }
}

/// The read side: a `KnowledgeSource` over the shared SQLite database.
#[derive(Debug)]
pub struct TeamSqliteMemorySource {
    path: Arc<PathBuf>,
    descriptor: KnowledgeSourceDescriptor,
}

/// The write side: a `MemoryStore` over the same SQLite database.
#[derive(Debug)]
pub struct TeamSqliteMemoryStore {
    path: Arc<PathBuf>,
    descriptor: MemoryStoreDescriptor,
}

impl TeamSqliteMemorySource {
    /// Open (creating if needed) the database at `db_path` and build both
    /// halves sharing it. The source registers with the `KnowledgeRegistry`
    /// and becomes the `MemoryService` read source; the store is the
    /// `MemoryService` write store.
    pub fn open(
        db_path: impl Into<PathBuf>,
    ) -> Result<(Arc<Self>, Arc<TeamSqliteMemoryStore>), KnowledgeError> {
        let path = Arc::new(db_path.into());
        initialize(&path)?;
        let descriptor = KnowledgeSourceDescriptor {
            id: TEAM_MEMORY_SOURCE_ID.to_string(),
            name: "Team Memory".to_string(),
            description: "Persistent lessons and facts recorded by team members".to_string(),
            domains: vec!["agent-team".to_string()],
            capabilities: BTreeSet::from([
                KnowledgeCapability::Search,
                KnowledgeCapability::Read,
                KnowledgeCapability::Revisioned,
            ]),
            freshness: FreshnessClass::Live,
            filter_fields: Vec::new(),
        };
        let source = Arc::new(Self {
            path: path.clone(),
            descriptor,
        });
        let store = Arc::new(TeamSqliteMemoryStore {
            path,
            descriptor: MemoryStoreDescriptor {
                read_source_id: TEAM_MEMORY_SOURCE_ID.to_string(),
                consistency: MemoryConsistency::Immediate,
            },
        });
        Ok((source, store))
    }
}

impl KnowledgeSource for TeamSqliteMemorySource {
    fn descriptor(&self) -> &KnowledgeSourceDescriptor {
        &self.descriptor
    }

    fn search<'a>(
        &'a self,
        _ctx: KnowledgeRequestContext<'a>,
        request: SourceSearchRequest,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<SourceSearchPage, KnowledgeError>> {
        Box::pin(async move {
            if abort.is_cancelled() {
                return Err(KnowledgeError::Aborted);
            }
            if !request.filters.is_empty() {
                return Err(KnowledgeError::InvalidFilter(
                    "team memory declares no filters".into(),
                ));
            }
            if request.cursor.is_some() {
                return Err(KnowledgeError::InvalidCursor);
            }
            let query = request.query.trim().to_lowercase();
            let hits = if query.is_empty() {
                Vec::new()
            } else {
                // OR-of-quoted-terms FTS query: any term matching is a hit;
                // bm25 rank orders the results.
                let terms: Vec<String> = query
                    .split_whitespace()
                    .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
                    .collect();
                let fts_query = terms.join(" OR ");
                let path = self.path.clone();
                let limit = request.limit.clamp(1, 50) as i64;
                tokio::task::spawn_blocking(move || -> Result<Vec<KnowledgeHit>, KnowledgeError> {
                    let connection = connect(&path)?;
                    let mut statement = connection
                        .prepare(
                            "SELECT m.item_id, m.revision, m.kind, m.created_at \
                             FROM memory_items_fts \
                             JOIN memory_items m ON m.item_id = memory_items_fts.item_id \
                             WHERE memory_items_fts MATCH ?1 \
                             ORDER BY bm25(memory_items_fts) LIMIT ?2",
                        )
                        .map_err(db_error)?;
                    let rows = statement
                        .query_map(rusqlite::params![fts_query, limit], |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, Option<String>>(2)?,
                                row.get::<_, String>(3)?,
                            ))
                        })
                        .map_err(db_error)?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(db_error)?;
                    Ok(rows
                        .into_iter()
                        .map(|(item_id, revision, kind, created_at)| KnowledgeHit {
                            reference: team_reference(&item_id, &revision),
                            title: Some(
                                kind.as_deref()
                                    .map(|k| format!("Team memory ({k})"))
                                    .unwrap_or_else(|| "Team memory".to_string()),
                            ),
                            snippet: TEAM_MEMORY_SNIPPET.to_string(),
                            suggested_selectors: vec![ContentSelector::Document],
                            uri: Some(format!("agent-team://memory/{item_id}")),
                            score: None,
                            updated_at: parse_created_at(&created_at),
                            metadata: Default::default(),
                        })
                        .collect())
                })
                .await
                .map_err(|_| KnowledgeError::Backend("team memory search task".into()))??
            };
            Ok(SourceSearchPage {
                hits,
                next_cursor: None,
            })
        })
    }

    fn read<'a>(
        &'a self,
        _ctx: KnowledgeRequestContext<'a>,
        request: KnowledgeReadRequest,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<KnowledgeContent, KnowledgeError>> {
        Box::pin(async move {
            if abort.is_cancelled() {
                return Err(KnowledgeError::Aborted);
            }
            if request.reference.source_id != TEAM_MEMORY_SOURCE_ID {
                return Err(KnowledgeError::NotFound);
            }
            let path = self.path.clone();
            let item_id = request.reference.item_id.clone();
            let expected_revision = request.reference.revision.clone();
            let selector = request.selector;
            tokio::task::spawn_blocking(move || -> Result<KnowledgeContent, KnowledgeError> {
                let connection = connect(&path)?;
                let row = connection
                    .query_row(
                        "SELECT revision, content, kind, created_at FROM memory_items \
                         WHERE item_id = ?1",
                        rusqlite::params![item_id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, Option<String>>(2)?,
                                row.get::<_, String>(3)?,
                            ))
                        },
                    )
                    .optional()
                    .map_err(db_error)?
                    .ok_or(KnowledgeError::NotFound)?;
                let (revision, content, kind, created_at) = row;
                if let Some(expected) = &expected_revision
                    && expected != &revision
                {
                    return Err(KnowledgeError::StaleReference {
                        latest: Some(team_reference(&item_id, &revision)),
                    });
                }
                Ok(KnowledgeContent {
                    reference: team_reference(&item_id, &revision),
                    selector,
                    title: Some(
                        kind.as_deref()
                            .map(|k| format!("Team memory ({k})"))
                            .unwrap_or_else(|| "Team memory".to_string()),
                    ),
                    blocks: vec![DataBlock::text(content)],
                    uri: Some(format!("agent-team://memory/{item_id}")),
                    updated_at: parse_created_at(&created_at),
                    obtained_at: Utc::now(),
                    truncated: false,
                    metadata: Default::default(),
                })
            })
            .await
            .map_err(|_| KnowledgeError::Backend("team memory read task".into()))?
        })
    }
}

impl RuntimeMemoryStore for TeamSqliteMemoryStore {
    fn descriptor(&self) -> &MemoryStoreDescriptor {
        &self.descriptor
    }

    fn upsert<'a>(
        &'a self,
        _ctx: KnowledgeRequestContext<'a>,
        write: MemoryWrite,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<MemoryWriteReceipt, KnowledgeError>> {
        Box::pin(async move {
            if abort.is_cancelled() {
                return Err(KnowledgeError::Aborted);
            }
            let path = self.path.clone();
            tokio::task::spawn_blocking(move || -> Result<MemoryWriteReceipt, KnowledgeError> {
                let revision =
                    llm_harness_knowledge_local::content_revision(write.content.as_bytes());
                let item_id = item_id_for(&write.idempotency_key);
                let connection = connect(&path)?;
                // Same idempotency key => same item id: the ON CONFLICT path
                // updates the row in place (new revision = new content
                // hash). The FTS triggers keep the search index in sync.
                connection
                    .execute(
                        "INSERT INTO memory_items \
                             (item_id, revision, content, kind, provenance_json, expires_at, created_at) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                         ON CONFLICT(item_id) DO UPDATE SET \
                             revision = excluded.revision, \
                             content = excluded.content, \
                             kind = excluded.kind, \
                             provenance_json = excluded.provenance_json, \
                             expires_at = excluded.expires_at",
                        rusqlite::params![
                            item_id,
                            revision,
                            write.content,
                            write.kind,
                            serde_json::to_string(&write.provenance)
                                .map_err(|e| KnowledgeError::Backend(e.to_string()))?,
                            write.expires_at.map(|t| t.to_rfc3339()),
                            Utc::now().to_rfc3339(),
                        ],
                    )
                    .map_err(db_error)?;
                // The INSERT .. ON CONFLICT fires the insert trigger only on
                // the insert path; on the update path re-index explicitly.
                connection
                    .execute(
                        "DELETE FROM memory_items_fts WHERE item_id = ?1",
                        rusqlite::params![item_id],
                    )
                    .map_err(db_error)?;
                connection
                    .execute(
                        "INSERT INTO memory_items_fts(item_id, content) VALUES (?1, ?2)",
                        rusqlite::params![item_id, write.content],
                    )
                    .map_err(db_error)?;
                Ok(MemoryWriteReceipt {
                    reference: team_reference(&item_id, &revision),
                    visibility: MemoryVisibility::Visible,
                })
            })
            .await
            .map_err(|_| KnowledgeError::Backend("team memory upsert task".into()))?
        })
    }

    fn delete<'a>(
        &'a self,
        _ctx: KnowledgeRequestContext<'a>,
        reference: KnowledgeRef,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<MemoryDeleteReceipt, KnowledgeError>> {
        Box::pin(async move {
            if abort.is_cancelled() {
                return Err(KnowledgeError::Aborted);
            }
            if reference.source_id != TEAM_MEMORY_SOURCE_ID {
                return Err(KnowledgeError::NotFound);
            }
            let path = self.path.clone();
            let item_id = reference.item_id.clone();
            let expected_revision = reference.revision.clone();
            tokio::task::spawn_blocking(move || -> Result<MemoryDeleteReceipt, KnowledgeError> {
                let connection = connect(&path)?;
                let latest: Option<String> = connection
                    .query_row(
                        "SELECT revision FROM memory_items WHERE item_id = ?1",
                        rusqlite::params![item_id],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(db_error)?;
                let Some(latest) = latest else {
                    return Err(KnowledgeError::NotFound);
                };
                // Optimistic delete: a stale reference must not remove a
                // newer revision written by another agent.
                if expected_revision.as_deref() != Some(latest.as_str()) {
                    return Err(KnowledgeError::StaleReference {
                        latest: Some(team_reference(&item_id, &latest)),
                    });
                }
                connection
                    .execute(
                        "DELETE FROM memory_items WHERE item_id = ?1",
                        rusqlite::params![item_id],
                    )
                    .map_err(db_error)?;
                connection
                    .execute(
                        "DELETE FROM memory_items_fts WHERE item_id = ?1",
                        rusqlite::params![item_id],
                    )
                    .map_err(db_error)?;
                Ok(MemoryDeleteReceipt {
                    reference,
                    visibility: MemoryVisibility::Visible,
                })
            })
            .await
            .map_err(|_| KnowledgeError::Backend("team memory delete task".into()))?
        })
    }
}

/// The `KnowledgeAccessContext` a team agent's run carries so memory tool
/// calls resolve their access context (`KnowledgeRequestContext::from_run`
/// looks the extension up; without it every call fails `Unauthorized`).
pub fn team_memory_access_context(agent_id: &str) -> KnowledgeAccessContext {
    KnowledgeAccessContext::new(
        KnowledgeScope::new(TEAM_MEMORY_SOURCE_ID),
        PrincipalRef::new(agent_id, "agent"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_memory::MemoryProvenance;
    use llm_harness_types::{RunContext, RunRequest};

    fn make_ctx() -> (RunContext, Arc<tempfile::TempDir>) {
        let dir = tempfile::tempdir().unwrap();
        let request =
            RunRequest::from_text("test").with_extension(team_memory_access_context("a1"));
        (RunContext::new(request), Arc::new(dir))
    }

    fn make_write(content: &str) -> MemoryWrite {
        MemoryWrite {
            content: content.to_string(),
            kind: Some("lesson".to_string()),
            metadata: Default::default(),
            provenance: MemoryProvenance {
                run_id: Default::default(),
                session_id: None,
                origin: llm_harness_memory::MemoryMutationOrigin::ExplicitTool {
                    tool_use_id: "test-tool-use".into(),
                },
                recorded_at: Utc::now(),
            },
            idempotency_key: format!("idem-{:016x}", {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                content.hash(&mut hasher);
                hasher.finish()
            }),
            expires_at: None,
        }
    }

    #[tokio::test]
    async fn upsert_then_search_finds_entry() {
        let (run, dir) = make_ctx();
        let (source, store) =
            TeamSqliteMemorySource::open(dir.path().join("memory.sqlite3")).unwrap();
        let ctx = KnowledgeRequestContext::from_run(&run).unwrap();

        let receipt = RuntimeMemoryStore::upsert(
            &*store,
            ctx,
            make_write("rust borrow checker lesson"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(receipt.reference.source_id, TEAM_MEMORY_SOURCE_ID);
        assert!(receipt.reference.item_id.starts_with("mem-"));

        let request2 =
            RunRequest::from_text("test").with_extension(team_memory_access_context("a1"));
        let run2 = RunContext::new(request2);
        let ctx2 = KnowledgeRequestContext::from_run(&run2).unwrap();
        let page = KnowledgeSource::search(
            &*source,
            ctx2,
            SourceSearchRequest {
                query: "borrow checker".into(),
                filters: vec![],
                limit: 10,
                cursor: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(page.hits.len(), 1, "search must find the written entry");
        assert_eq!(page.hits[0].reference.item_id, receipt.reference.item_id);
        assert_eq!(page.hits[0].reference.revision, receipt.reference.revision);
    }

    #[tokio::test]
    async fn read_returns_content_and_enforces_revision() {
        let (run, dir) = make_ctx();
        let (source, store) =
            TeamSqliteMemorySource::open(dir.path().join("memory.sqlite3")).unwrap();
        let ctx = KnowledgeRequestContext::from_run(&run).unwrap();
        let receipt = RuntimeMemoryStore::upsert(
            &*store,
            ctx,
            make_write("prefer async tests in this codebase"),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        // Read with the current revision succeeds and returns the content.
        let request2 = RunRequest::from_text("t").with_extension(team_memory_access_context("a1"));
        let run2 = RunContext::new(request2);
        let ctx2 = KnowledgeRequestContext::from_run(&run2).unwrap();
        let content = KnowledgeSource::read(
            &*source,
            ctx2,
            KnowledgeReadRequest {
                reference: receipt.reference.clone(),
                selector: ContentSelector::Document,
                max_bytes: 64 * 1024,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(content.blocks.iter().any(|b| match b {
            DataBlock::Text { text, .. } => text.contains("prefer async tests"),
            _ => false,
        }));

        // A stale revision yields StaleReference carrying the latest ref.
        let stale = KnowledgeRef {
            revision: Some("bogus".to_string()),
            ..receipt.reference.clone()
        };
        let request3 = RunRequest::from_text("t").with_extension(team_memory_access_context("a1"));
        let run3 = RunContext::new(request3);
        let ctx3 = KnowledgeRequestContext::from_run(&run3).unwrap();
        let err = KnowledgeSource::read(
            &*source,
            ctx3,
            KnowledgeReadRequest {
                reference: stale,
                selector: ContentSelector::Document,
                max_bytes: 64 * 1024,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, KnowledgeError::StaleReference { .. }));
    }

    #[tokio::test]
    async fn delete_enforces_revision_and_removes_entry() {
        let (run, dir) = make_ctx();
        let (source, store) =
            TeamSqliteMemorySource::open(dir.path().join("memory.sqlite3")).unwrap();
        let ctx = KnowledgeRequestContext::from_run(&run).unwrap();
        let receipt = RuntimeMemoryStore::upsert(
            &*store,
            ctx,
            make_write("entry to be forgotten"),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        // Stale delete is rejected with the latest reference.
        let stale = KnowledgeRef {
            revision: Some("old".to_string()),
            ..receipt.reference.clone()
        };
        let err = RuntimeMemoryStore::delete(
            &*store,
            KnowledgeRequestContext::from_run(&RunContext::new(
                RunRequest::from_text("t").with_extension(team_memory_access_context("a1")),
            ))
            .unwrap(),
            stale,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, KnowledgeError::StaleReference { .. }));

        // Delete with the current revision removes the entry...
        let receipt2 = RuntimeMemoryStore::delete(
            &*store,
            KnowledgeRequestContext::from_run(&RunContext::new(
                RunRequest::from_text("t").with_extension(team_memory_access_context("a1")),
            ))
            .unwrap(),
            receipt.reference.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("delete2 failed: {e:?} / {e}"));
        assert_eq!(receipt2.reference.item_id, receipt.reference.item_id);
        eprintln!("DBG delete2 ok");

        // ...and it disappears from search.
        let page = KnowledgeSource::search(
            &*source,
            KnowledgeRequestContext::from_run(&RunContext::new(
                RunRequest::from_text("t").with_extension(team_memory_access_context("a1")),
            ))
            .unwrap(),
            SourceSearchRequest {
                query: "forgotten".into(),
                filters: vec![],
                limit: 10,
                cursor: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(page.hits.is_empty(), "deleted entry must leave the index");

        // Double delete is NotFound.
        let err = RuntimeMemoryStore::delete(
            &*store,
            KnowledgeRequestContext::from_run(&RunContext::new(
                RunRequest::from_text("t").with_extension(team_memory_access_context("a1")),
            ))
            .unwrap(),
            receipt.reference.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, KnowledgeError::NotFound));
    }

    #[tokio::test]
    async fn same_idempotency_key_updates_in_place() {
        let (run, dir) = make_ctx();
        let (source, store) =
            TeamSqliteMemorySource::open(dir.path().join("memory.sqlite3")).unwrap();
        let ctx = KnowledgeRequestContext::from_run(&run).unwrap();

        let first = RuntimeMemoryStore::upsert(
            &*store,
            ctx,
            make_write("lesson v1"),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        // A second write with the SAME idempotency key but new content must
        // update the existing item (its id stays), producing a new revision.
        let mut second = make_write("lesson v1 placeholder");
        second.idempotency_key = first
            .reference
            .item_id
            .strip_prefix("mem-")
            .expect("mem- prefix")
            .to_string();
        // Derive the write the same way the policy would: rebuild with the
        // same key by overwriting it after constructing the content hash.
        let updated = MemoryWrite {
            content: "lesson with corrected wording".to_string(),
            ..second
        };
        let second_receipt = RuntimeMemoryStore::upsert(
            &*store,
            KnowledgeRequestContext::from_run(&RunContext::new(
                RunRequest::from_text("t").with_extension(team_memory_access_context("a1")),
            ))
            .unwrap(),
            updated,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            first.reference.item_id, second_receipt.reference.item_id,
            "same idempotency key converges to the same item"
        );
        assert_ne!(
            first.reference.revision, second_receipt.reference.revision,
            "content change must produce a new revision"
        );

        // Reading the item by the new reference returns the new content.
        let content = KnowledgeSource::read(
            &*source,
            KnowledgeRequestContext::from_run(&RunContext::new(
                RunRequest::from_text("t").with_extension(team_memory_access_context("a1")),
            ))
            .unwrap(),
            KnowledgeReadRequest {
                reference: second_receipt.reference.clone(),
                selector: ContentSelector::Document,
                max_bytes: 64 * 1024,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(content.blocks.iter().any(|b| match b {
            DataBlock::Text { text, .. } => text.contains("corrected wording"),
            _ => false,
        }));
    }

    #[tokio::test]
    async fn database_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nested").join("memory.sqlite3");
        let (run, _keep) = make_ctx();
        {
            let (source, store) = TeamSqliteMemorySource::open(&db_path).unwrap();
            let ctx = KnowledgeRequestContext::from_run(&run).unwrap();
            RuntimeMemoryStore::upsert(
                &*store,
                ctx,
                make_write("durable lesson across restarts"),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            drop(source);
            drop(store);
        }
        // Re-open: the entry is still searchable (persistence proof).
        let (source, _store) = TeamSqliteMemorySource::open(&db_path).unwrap();
        let request = RunRequest::from_text("t").with_extension(team_memory_access_context("a1"));
        let run2 = RunContext::new(request);
        let page = KnowledgeSource::search(
            &*source,
            KnowledgeRequestContext::from_run(&run2).unwrap(),
            SourceSearchRequest {
                query: "durable lesson".into(),
                filters: vec![],
                limit: 10,
                cursor: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(page.hits.len(), 1, "entry must survive reopen");
    }

    #[tokio::test]
    async fn foreign_database_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("foreign.sqlite3");
        {
            let connection = Connection::open(&db_path).unwrap();
            connection
                .execute_batch("PRAGMA application_id = 12345; PRAGMA user_version = 5;")
                .unwrap();
        }
        let err = TeamSqliteMemorySource::open(&db_path).unwrap_err();
        assert!(
            matches!(err, KnowledgeError::Backend(_)),
            "foreign database must be rejected, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn empty_search_query_returns_no_hits() {
        let (run, dir) = make_ctx();
        let (source, store) =
            TeamSqliteMemorySource::open(dir.path().join("memory.sqlite3")).unwrap();
        let ctx = KnowledgeRequestContext::from_run(&run).unwrap();
        RuntimeMemoryStore::upsert(
            &*store,
            ctx,
            make_write("some entry"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let page = KnowledgeSource::search(
            &*source,
            KnowledgeRequestContext::from_run(&RunContext::new(
                RunRequest::from_text("t").with_extension(team_memory_access_context("a1")),
            ))
            .unwrap(),
            SourceSearchRequest {
                query: "   ".into(),
                filters: vec![],
                limit: 10,
                cursor: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(page.hits.is_empty());
    }

    #[tokio::test]
    async fn reflection_loop_closure_write_then_dedupe_search() {
        // The end-to-end property the persistence work exists for: a lesson
        // written via memory_write is found by the SAME query a later
        // reflection would issue through knowledge_search — across a
        // store/source round-trip (simulating a later process).
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("memory.sqlite3");

        // Process 1: reflection writes the lesson.
        {
            let (source, store) = TeamSqliteMemorySource::open(&db_path).unwrap();
            let request =
                RunRequest::from_text("t").with_extension(team_memory_access_context("a1"));
            let run = RunContext::new(request);
            let ctx = KnowledgeRequestContext::from_run(&run).unwrap();
            RuntimeMemoryStore::upsert(
                &*store,
                ctx,
                make_write("prefer cargo fmt before every commit"),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            drop(source);
            drop(store);
        }

        // Process 2: the next reflection dedupes via knowledge_search.
        let (source, _store) = TeamSqliteMemorySource::open(&db_path).unwrap();
        let request2 = RunRequest::from_text("t").with_extension(team_memory_access_context("a1"));
        let run2 = RunContext::new(request2);
        let page = KnowledgeSource::search(
            &*source,
            KnowledgeRequestContext::from_run(&run2).unwrap(),
            SourceSearchRequest {
                query: "cargo fmt commit".into(),
                filters: vec![],
                limit: 10,
                cursor: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(page.hits.len(), 1, "written lesson must be findable");
        assert!(
            page.hits[0]
                .title
                .as_deref()
                .unwrap_or_default()
                .contains("lesson")
        );
    }
}
