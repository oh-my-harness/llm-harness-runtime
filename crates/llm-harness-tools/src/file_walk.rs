//! Shared recursive directory walk helper for grep and glob tools.
//!
//! Walks a directory tree via [`ExecutionEnv::list_dir`], returning file
//! entries (directories themselves are excluded from results).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use futures::future::BoxFuture;
use llm_harness_types::{EnvError, ExecutionEnv, FileInfo};
use tokio_util::sync::CancellationToken;

/// Maximum recursion depth to prevent stack overflow on excessively deep
/// directory trees.
const MAX_DEPTH: u32 = 64;

/// Recursively walk a directory, collecting only files matching `predicate`.
///
/// Returns `(matched_files, total_matched)` where `total_matched` counts all
/// files that matched the predicate, while the returned vector contains at
/// most `max_results` entries.
///
/// The walk traverses the entire tree (no early termination) so that
/// `total_matched` is exact — callers can distinguish "exactly N matches"
/// from "N+ matches, capped at N" via `total_matched > max_results`.
///
/// Directories themselves are excluded — only files are returned. A depth
/// limit ([`MAX_DEPTH`]) guards against excessively deep trees, and a
/// visited-set prevents infinite loops through symlink cycles.
pub(crate) async fn walk_dir_filtered(
    env: &dyn ExecutionEnv,
    root: &Path,
    abort: &CancellationToken,
    predicate: &(dyn Fn(&FileInfo) -> bool + Send + Sync),
    max_results: usize,
) -> Result<(Vec<FileInfo>, usize), EnvError> {
    let mut results = Vec::with_capacity(max_results.min(1024));
    let mut total_matched = 0usize;
    let mut visited: HashSet<PathBuf> = HashSet::new();
    walk_dir_inner(
        env,
        root,
        abort,
        0,
        &mut results,
        &mut total_matched,
        predicate,
        &max_results,
        &mut visited,
    )
    .await?;
    Ok((results, total_matched))
}

#[allow(clippy::too_many_arguments)]
fn walk_dir_inner<'a>(
    env: &'a dyn ExecutionEnv,
    dir: &'a Path,
    abort: &'a CancellationToken,
    depth: u32,
    results: &'a mut Vec<FileInfo>,
    total_matched: &'a mut usize,
    predicate: &'a (dyn Fn(&FileInfo) -> bool + Send + Sync),
    max_results: &'a usize,
    visited: &'a mut HashSet<PathBuf>,
) -> BoxFuture<'a, Result<(), EnvError>> {
    Box::pin(async move {
        if depth >= MAX_DEPTH {
            return Ok(());
        }
        // Guard against symlink loops: skip directories already visited
        // on this traversal.
        if !visited.insert(dir.to_path_buf()) {
            return Ok(());
        }
        let entries = env.list_dir(dir, abort.clone()).await?;
        for entry in entries {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            if entry.is_dir {
                // Recurse into subdirectory.
                walk_dir_inner(
                    env,
                    &entry.path,
                    abort,
                    depth + 1,
                    results,
                    total_matched,
                    predicate,
                    max_results,
                    visited,
                )
                .await?;
            } else if predicate(&entry) {
                // Count ALL matches (even those not stored due to the cap)
                // so callers can distinguish "exactly N matches" from "N+
                // matches, capped at N".
                *total_matched += 1;
                if results.len() < *max_results {
                    results.push(entry);
                }
            }
        }
        Ok(())
    })
}
