use std::{hint::black_box, path::Path, time::Instant};

use chrono::Utc;
use llm_harness_agent::SessionRecallScope;
use llm_harness_runtime_session_recall::{
    SessionRecallDocument, SessionRecallIndex, SessionRecallIndexQuery, SessionRecallProjection,
    SessionRecallRef, SqliteSessionRecallIndex,
};
use llm_harness_types::EntryId;
use tokio_util::sync::CancellationToken;

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("benchmark runtime");
    runtime.block_on(run());
}

async fn run() {
    let session_count = env_usize("SESSION_RECALL_BENCH_SESSIONS", 1_000);
    let documents_per_session = env_usize("SESSION_RECALL_BENCH_DOCS_PER_SESSION", 5);
    let search_count = env_usize("SESSION_RECALL_BENCH_SEARCHES", 200);
    let directory = tempfile::tempdir().expect("benchmark directory");
    let path = directory.path().join("session-recall.sqlite3");
    let index = SqliteSessionRecallIndex::open(&path).expect("open benchmark index");
    let projections = projections(session_count, documents_per_session);

    let indexing_started = Instant::now();
    index
        .replace_all(projections)
        .await
        .expect("index benchmark generation");
    let indexing_millis = indexing_started.elapsed().as_millis();

    let mut search_micros = Vec::with_capacity(search_count);
    for query_index in 0..search_count {
        let started = Instant::now();
        let hits = index
            .search(
                SessionRecallIndexQuery {
                    query: format!("aurora benchmark bucket {}", query_index % 100),
                    scope: SessionRecallScope::new("benchmark"),
                    exclude_session_id: None,
                    limit: 10,
                    max_snippet_bytes: 1_024,
                },
                CancellationToken::new(),
            )
            .await
            .expect("benchmark search");
        black_box(hits);
        search_micros.push(started.elapsed().as_micros());
    }
    search_micros.sort_unstable();

    drop(index);
    let reopen_started = Instant::now();
    let reopened = SqliteSessionRecallIndex::open(&path).expect("reopen benchmark index");
    let revision_count = reopened
        .projected_revisions()
        .await
        .expect("read projected revisions")
        .len();
    let reopen_and_revision_scan_millis = reopen_started.elapsed().as_millis();

    println!(
        "{{\"sessions\":{session_count},\"documents\":{},\"indexing_ms\":{indexing_millis},\"index_bytes\":{},\"searches\":{search_count},\"search_p50_us\":{},\"search_p95_us\":{},\"search_p99_us\":{},\"reopen_revision_count\":{revision_count},\"reopen_revision_scan_ms\":{reopen_and_revision_scan_millis}}}",
        session_count.saturating_mul(documents_per_session),
        sqlite_bytes(&path),
        percentile(&search_micros, 50),
        percentile(&search_micros, 95),
        percentile(&search_micros, 99),
    );
}

fn projections(session_count: usize, documents_per_session: usize) -> Vec<SessionRecallProjection> {
    (0..session_count)
        .map(|session_index| {
            let session_id = format!("benchmark-session-{session_index:08}");
            let branch_leaf_id = EntryId::new();
            let documents = (0..documents_per_session)
                .map(|document_index| SessionRecallDocument {
                    reference: SessionRecallRef {
                        session_id: session_id.clone(),
                        user_entry_id: EntryId::new(),
                        assistant_entry_id: Some(EntryId::new()),
                        branch_leaf_id,
                        revision: 1,
                    },
                    title: Some(format!("Benchmark session {session_index}")),
                    user_text: format!(
                        "Aurora benchmark bucket {} question {document_index} for session {session_index}",
                        session_index % 100
                    ),
                    assistant_text: Some(format!(
                        "Benchmark answer {document_index} retains deterministic searchable text"
                    )),
                    updated_at: Utc::now(),
                })
                .collect();
            SessionRecallProjection {
                session_id,
                revision: 1,
                scope: SessionRecallScope::new("benchmark"),
                documents,
            }
        })
        .collect()
}

fn percentile(samples: &[u128], percentile: usize) -> u128 {
    if samples.is_empty() {
        return 0;
    }
    let index = (samples.len() - 1).saturating_mul(percentile) / 100;
    samples[index]
}

fn sqlite_bytes(path: &Path) -> u64 {
    [
        path.to_path_buf(),
        suffix(path, "-wal"),
        suffix(path, "-shm"),
    ]
    .into_iter()
    .filter_map(|path| std::fs::metadata(path).ok())
    .map(|metadata| metadata.len())
    .sum()
}

fn suffix(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    value.into()
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}
