use std::collections::{BTreeMap, BTreeSet};

use futures::future::BoxFuture;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::{
    SessionRecallCandidate, SessionRecallError, SessionRecallIndexQuery, SessionRecallProjection,
    truncate_utf8,
};

pub trait SessionRecallIndex: Send + Sync {
    fn search(
        &self,
        request: SessionRecallIndexQuery,
        abort: CancellationToken,
    ) -> BoxFuture<'_, Result<Vec<SessionRecallCandidate>, SessionRecallError>>;

    fn replace_session(
        &self,
        projection: SessionRecallProjection,
    ) -> BoxFuture<'_, Result<bool, SessionRecallError>>;

    fn remove_session(
        &self,
        session_id: String,
        revision: u64,
        permanent: bool,
    ) -> BoxFuture<'_, Result<bool, SessionRecallError>>;

    fn projected_revisions(
        &self,
    ) -> BoxFuture<'_, Result<BTreeMap<String, u64>, SessionRecallError>>;

    /// Atomically replace the complete derived generation.
    fn replace_all(
        &self,
        projections: Vec<SessionRecallProjection>,
    ) -> BoxFuture<'_, Result<(), SessionRecallError>>;

    fn reset(&self) -> BoxFuture<'_, Result<(), SessionRecallError>>;
}

#[derive(Debug, Clone, Copy)]
enum Tombstone {
    Revision(u64),
    Permanent,
}

#[derive(Default)]
struct IndexState {
    sessions: BTreeMap<String, SessionRecallProjection>,
    tombstones: BTreeMap<String, Tombstone>,
}

/// Dependency-free lexical index intended for tests and small deployments.
#[derive(Default)]
pub struct InMemorySessionRecallIndex {
    state: RwLock<IndexState>,
}

impl InMemorySessionRecallIndex {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SessionRecallIndex for InMemorySessionRecallIndex {
    fn search(
        &self,
        request: SessionRecallIndexQuery,
        abort: CancellationToken,
    ) -> BoxFuture<'_, Result<Vec<SessionRecallCandidate>, SessionRecallError>> {
        Box::pin(async move {
            if abort.is_cancelled() {
                return Err(SessionRecallError::Aborted);
            }
            if request.limit == 0 || request.max_snippet_bytes == 0 {
                return Ok(vec![]);
            }
            let terms = terms(&request.query);
            if terms.is_empty() {
                return Ok(vec![]);
            }
            let state = self.state.read().await;
            let mut candidates = Vec::new();
            for projection in state.sessions.values() {
                if abort.is_cancelled() {
                    return Err(SessionRecallError::Aborted);
                }
                if projection.scope != request.scope
                    || request
                        .exclude_session_id
                        .as_ref()
                        .is_some_and(|id| id == &projection.session_id)
                {
                    continue;
                }
                for document in &projection.documents {
                    let haystack = document.searchable_text().to_lowercase();
                    let score = terms
                        .iter()
                        .map(|term| haystack.matches(term).count() as f32)
                        .sum::<f32>();
                    if score == 0.0 {
                        continue;
                    }
                    let (snippet, _) =
                        truncate_utf8(&document.formatted_content(), request.max_snippet_bytes);
                    candidates.push(SessionRecallCandidate {
                        reference: document.reference.clone(),
                        title: document.title.clone(),
                        snippet,
                        score,
                        updated_at: document.updated_at,
                    });
                }
            }
            candidates.sort_by(|a, b| {
                b.score
                    .total_cmp(&a.score)
                    .then_with(|| b.updated_at.cmp(&a.updated_at))
                    .then_with(|| a.reference.cmp(&b.reference))
            });
            candidates.truncate(request.limit);
            Ok(candidates)
        })
    }

    fn replace_session(
        &self,
        projection: SessionRecallProjection,
    ) -> BoxFuture<'_, Result<bool, SessionRecallError>> {
        Box::pin(async move {
            let mut state = self.state.write().await;
            if matches!(
                state.tombstones.get(&projection.session_id),
                Some(Tombstone::Permanent)
            ) {
                return Ok(false);
            }
            if let Some(Tombstone::Revision(revision)) =
                state.tombstones.get(&projection.session_id)
                && *revision >= projection.revision
            {
                return Ok(false);
            }
            if state
                .sessions
                .get(&projection.session_id)
                .is_some_and(|stored| stored.revision >= projection.revision)
            {
                return Ok(false);
            }
            state.tombstones.remove(&projection.session_id);
            state
                .sessions
                .insert(projection.session_id.clone(), projection);
            Ok(true)
        })
    }

    fn remove_session(
        &self,
        session_id: String,
        revision: u64,
        permanent: bool,
    ) -> BoxFuture<'_, Result<bool, SessionRecallError>> {
        Box::pin(async move {
            let mut state = self.state.write().await;
            if state
                .sessions
                .get(&session_id)
                .is_some_and(|stored| stored.revision > revision)
                && !permanent
            {
                return Ok(false);
            }
            state.sessions.remove(&session_id);
            let tombstone = if permanent {
                Tombstone::Permanent
            } else {
                match state.tombstones.get(&session_id) {
                    Some(Tombstone::Permanent) => Tombstone::Permanent,
                    Some(Tombstone::Revision(current)) => {
                        Tombstone::Revision((*current).max(revision))
                    }
                    None => Tombstone::Revision(revision),
                }
            };
            state.tombstones.insert(session_id, tombstone);
            Ok(true)
        })
    }

    fn projected_revisions(
        &self,
    ) -> BoxFuture<'_, Result<BTreeMap<String, u64>, SessionRecallError>> {
        Box::pin(async move {
            Ok(self
                .state
                .read()
                .await
                .sessions
                .iter()
                .map(|(id, projection)| (id.clone(), projection.revision))
                .collect())
        })
    }

    fn replace_all(
        &self,
        projections: Vec<SessionRecallProjection>,
    ) -> BoxFuture<'_, Result<(), SessionRecallError>> {
        Box::pin(async move {
            let mut sessions: BTreeMap<String, SessionRecallProjection> = BTreeMap::new();
            for projection in projections {
                match sessions.get(&projection.session_id) {
                    Some(current) if current.revision > projection.revision => {}
                    _ => {
                        sessions.insert(projection.session_id.clone(), projection);
                    }
                }
            }
            *self.state.write().await = IndexState {
                sessions,
                tombstones: BTreeMap::new(),
            };
            Ok(())
        })
    }

    fn reset(&self) -> BoxFuture<'_, Result<(), SessionRecallError>> {
        Box::pin(async move {
            *self.state.write().await = IndexState::default();
            Ok(())
        })
    }
}

fn terms(query: &str) -> BTreeSet<String> {
    query
        .split(|character: char| !character.is_alphanumeric())
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(str::to_lowercase)
        .collect()
}
