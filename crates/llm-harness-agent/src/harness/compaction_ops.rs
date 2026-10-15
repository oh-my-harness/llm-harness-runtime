// compaction_ops.rs — compaction methods extracted from harness/mod.rs
use std::sync::Arc;

use llm_harness_loop::{LlmClient, composite::CompositeBeforeCompactHook};
use llm_harness_types::{
    BeforeCompactCtx, BeforeCompactDecision, BeforeCompactHook, CompactionError, CompactionResult,
    EntryId, HarnessError, HarnessPhase,
};
use tokio_util::sync::CancellationToken;

use crate::agent::ModelInfo;
use crate::compaction::{
    CompactionSummaryConfig, CompactionThresholdConfig, DEFAULT_CONTEXT_WINDOW, FileOperation,
    compact, compute_cut_point, estimate_summary_tokens, estimate_tokens,
};
use crate::session::types::{CompactionEntry, SessionEntry, SessionEntryPayload};

use super::AgentHarness;
use super::event::{AgentHarnessEvent, CompactionStats};
use super::phase_guard::AbortTokenGuard;

/// Find the index in `path` of the `first_kept_entry` belonging to the most
/// recent `Compaction` entry. Returns 0 when there is no prior compaction.
fn live_region_start(path: &[SessionEntry]) -> usize {
    path.iter()
        .rev()
        .find_map(|e| {
            if let SessionEntryPayload::Compaction(c) = &e.payload {
                Some(c.first_kept_entry)
            } else {
                None
            }
        })
        .and_then(|id| path.iter().position(|e| e.id == id))
        .unwrap_or(0)
}

// ── CompactionPrereqs ─────────────────────────────────────────────────────────

/// Shared compaction prerequisites computed once from the session path and
/// harness config. Used by `try_auto_compact` (pre-check) and `do_compact`
/// (execution) to avoid redundant computation.
struct CompactionPrereqs {
    last_compaction: Option<CompactionEntry>,
    threshold_config: CompactionThresholdConfig,
    summary_config: CompactionSummaryConfig,
    summary_client: Arc<dyn LlmClient>,
    start_idx: usize,
    estimated_tokens: usize,
    threshold: usize,
}

impl AgentHarness {
    /// Run compaction on the active session path (Idle only).
    ///
    /// Enters `Compacting` phase, runs `do_compact`, emits `CompactionEnd`,
    /// and returns to `Idle` when the phase guard drops.
    ///
    /// Event contract: `CompactionEnd` is emitted while the harness is still
    /// in the `Compacting` phase (before the guard drops and restores Idle).
    /// Consumers observing `PhaseChange { Compacting → Idle }` can rely on the
    /// `CompactionEnd` event having already been emitted.
    ///
    /// If `auto_compact` is enabled, this is called automatically at
    /// `prompt_with_messages` entry.
    ///
    /// Not subject to the circuit breaker — manual compaction always runs.
    /// On success, resets the compaction failure count (and thus the circuit
    /// breaker if it was tripped). On failure, the count is not incremented.
    pub async fn compact(&self) -> Result<CompactionStats, HarnessError> {
        self.compact_with_path(None).await
    }

    /// Reset the compaction circuit breaker.
    ///
    /// After the circuit breaker trips (`CircuitOpen`), auto-compact is
    /// blocked until the failure count is reset. Manual `compact()` is
    /// unaffected and can still run, but if it also fails the count is
    /// not reset.
    ///
    /// Call this after fixing the underlying cause (e.g. switching to a
    /// working summary model) to re-enable auto-compact.
    pub fn reset_circuit_breaker(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.compaction_failure_count = 0;
    }

    /// Update the compaction query intent at runtime.
    ///
    /// Allows long-lived harness instances to switch task context between
    /// different prompts. The value is used by the next auto-compact or
    /// manual `compact()` invocation.
    ///
    /// See [`AgentHarnessOptions::compaction_query`] for trust boundary.
    pub fn set_compaction_query(&self, query: Option<String>) {
        let mut inner = self.inner.lock().unwrap();
        inner.compaction_query = query;
    }

    /// Internal compaction entry point that accepts an optional pre-loaded
    /// session path to avoid redundant `read_active_path` I/O when the caller
    /// already has the data (e.g. `try_auto_compact`).
    async fn compact_with_path(
        &self,
        preloaded_path: Option<Vec<SessionEntry>>,
    ) -> Result<CompactionStats, HarnessError> {
        let _guard = self.enter_phase(HarnessPhase::Compacting)?;
        let result = self.do_compact(preloaded_path, None).await;
        match result {
            Ok(stats) => {
                {
                    let mut inner = self.inner.lock().unwrap();
                    inner.compaction_failure_count = 0;
                }
                self.emit(AgentHarnessEvent::CompactionEnd {
                    stats: Some(stats.clone()),
                    error: None,
                });
                Ok(stats)
            }
            Err(e) => {
                let msg = e.to_string();
                self.emit(AgentHarnessEvent::CompactionEnd {
                    stats: None,
                    error: Some(msg.clone()),
                });
                Err(e)
            }
        }
    }

    /// Returns the current compaction query, if set.
    fn current_query_for_compaction(&self) -> Option<String> {
        let inner = self.inner.lock().unwrap();
        inner.compaction_query.clone()
    }

    /// Auto-compaction check called at `prompt_with_messages` entry (Idle phase).
    ///
    /// If `auto_compact` is disabled, returns `Ok(())` immediately.
    /// Performs a lightweight threshold check *before* entering the Compacting
    /// phase to avoid spurious `PhaseChange`/`CompactionEnd` events on every
    /// turn where compaction is not needed.
    ///
    /// When a `before_compact` hook is registered, the threshold pre-check is
    /// skipped — the hook may return `BeforeCompactDecision::Compact` to force
    /// compaction even when tokens are under threshold.
    ///
    /// Returns `Ok(())` when compaction is not needed (`InsufficientTokens`)
    /// or when no valid cut boundary exists but tokens are within the context
    /// window (soft threshold exceedance — the request fits, so the run
    /// proceeds silently). When tokens exceed the context window and no valid
    /// cut boundary exists, returns `Err(NoValidBoundary)` — continuing would
    /// send an over-limit request to the API.
    ///
    /// Unlike `compact_with_path` (used by the public `compact()` method),
    /// benign skips (`InsufficientTokens` / `NoValidBoundary` under the
    /// context window) do NOT emit `CompactionEnd` events — they are silently
    /// treated as "nothing to do". This avoids spurious error events when a
    /// hook forces `Compact` but no valid cut boundary exists.
    ///
    /// # Circuit breaker scope
    ///
    /// The circuit breaker only tracks `SummaryFailed` errors (LLM summary
    /// generation failures). Other error types — `Session` (storage faults),
    /// `InvalidFirstKeptEntry` (logic errors), `Agent` — propagate immediately
    /// without incrementing the failure count. These are non-transient: retrying
    /// won't help a full disk or a malformed session, so they should surface
    /// right away rather than accumulate toward the circuit breaker threshold.
    pub(super) async fn try_auto_compact(&self) -> Result<(), HarnessError> {
        let (auto_compact, has_before_compact_hook) = {
            let inner = self.inner.lock().unwrap();
            (inner.auto_compact, !self.hooks.before_compact.is_empty())
        };
        if !auto_compact {
            return Ok(());
        }

        // Circuit breaker: if consecutive failures have tripped the breaker,
        // refuse to auto-compact.
        {
            let inner = self.inner.lock().unwrap();
            let max = inner.compaction_max_failures;
            if inner.compaction_failure_count >= max {
                return Err(CompactionError::CircuitOpen(inner.compaction_failure_count).into());
            }
        }

        // Lightweight pre-check: if tokens are under threshold, skip without
        // entering Compacting phase (avoids spurious events).
        let path = self.session.read_active_path().await?;
        if path.is_empty() {
            return Ok(());
        }

        let pre = self.compaction_prereqs(&path);

        if !has_before_compact_hook {
            // No hook: apply threshold pre-check and cut-point validation to
            // avoid spurious PhaseChange/CompactionEnd events.
            if pre.estimated_tokens <= pre.threshold {
                return Ok(());
            }
            // Validate that a valid cut boundary exists before entering the
            // Compacting phase — otherwise do_compact would return
            // NoValidBoundary after emitting spurious events.
            if compute_cut_point(
                &path,
                pre.last_compaction.as_ref(),
                &pre.threshold_config,
                Some(pre.start_idx),
                Some(pre.estimated_tokens),
            )
            .is_none()
            {
                return self.handle_no_valid_boundary(
                    pre.estimated_tokens,
                    pre.threshold_config.context_window as usize,
                );
            }
        }

        // Save values needed for NoValidBoundary handling before `pre` is
        // moved into do_compact.
        let saved_estimated_tokens = pre.estimated_tokens;
        let saved_context_window = pre.threshold_config.context_window as usize;

        // Create a temporary abort token so `abort()` can cancel the
        // compaction LLM call during auto-compaction (run_loop hasn't set
        // current_abort yet). The RAII guard restores the previous value on
        // drop, making it panic-safe.
        let _abort_guard = AbortTokenGuard::new(self, CancellationToken::new());

        // Enter Compacting phase and run do_compact directly (not via
        // compact_with_path) so benign skips don't emit CompactionEnd events.
        let _guard = self.enter_phase(HarnessPhase::Compacting)?;
        let result = self.do_compact(Some(path), Some(pre)).await;
        match result {
            Ok(stats) => {
                // Success: reset failure count.
                {
                    let mut inner = self.inner.lock().unwrap();
                    inner.compaction_failure_count = 0;
                }
                self.emit(AgentHarnessEvent::CompactionEnd {
                    stats: Some(stats),
                    error: None,
                });
                Ok(())
            }
            // Benign skips: no CompactionEnd event, no error propagation.
            Err(HarnessError::Compaction(CompactionError::InsufficientTokens)) => Ok(()),
            Err(HarnessError::Compaction(CompactionError::NoValidBoundary)) => {
                // do_compact returned NoValidBoundary after entering Compacting
                // phase. Apply the same over-limit logic: if tokens exceed the
                // context window, propagate the error; otherwise silently proceed.
                self.handle_no_valid_boundary(saved_estimated_tokens, saved_context_window)
            }
            Err(HarnessError::Compaction(CompactionError::Skipped)) => Ok(()),
            Err(e)
                if matches!(
                    &e,
                    HarnessError::Compaction(CompactionError::SummaryFailed(_))
                ) =>
            {
                let (count, max) = {
                    let mut inner = self.inner.lock().unwrap();
                    inner.compaction_failure_count += 1;
                    (
                        inner.compaction_failure_count,
                        inner.compaction_max_failures,
                    )
                };
                if count >= max {
                    // Circuit just tripped — return CircuitOpen.
                    self.emit(AgentHarnessEvent::CompactionEnd {
                        stats: None,
                        error: Some(CompactionError::CircuitOpen(count).to_string()),
                    });
                    return Err(CompactionError::CircuitOpen(count).into());
                }
                self.emit(AgentHarnessEvent::CompactionEnd {
                    stats: None,
                    error: Some(e.to_string()),
                });
                // Circuit not yet tripped. If the context still exceeds the
                // model's window, we cannot proceed — the main request would
                // be rejected by the provider. Propagate as NoValidBoundary.
                if saved_estimated_tokens > saved_context_window {
                    return Err(CompactionError::NoValidBoundary.into());
                }
                // Context fits within the window — swallow the error so the
                // run proceeds. The circuit breaker will trip after `max`
                // consecutive failures.
                Ok(())
            }
            Err(e) => {
                self.emit(AgentHarnessEvent::CompactionEnd {
                    stats: None,
                    error: Some(e.to_string()),
                });
                Err(e)
            }
        }
    }

    /// Handle the case where compaction is needed but no valid cut boundary
    /// exists.
    ///
    /// - If `estimated_tokens > context_window`: returns `Err(NoValidBoundary)`
    ///   because continuing would send an over-limit request to the API.
    /// - If `estimated_tokens <= context_window`: returns `Ok(())` silently —
    ///   the request fits within the model's capacity even though it exceeds
    ///   the reserve. No event emitted, consistent with InsufficientTokens/Skipped.
    fn handle_no_valid_boundary(
        &self,
        estimated_tokens: usize,
        context_window: usize,
    ) -> Result<(), HarnessError> {
        if estimated_tokens > context_window {
            Err(CompactionError::NoValidBoundary.into())
        } else {
            Ok(())
        }
    }

    async fn do_compact(
        &self,
        preloaded_path: Option<Vec<SessionEntry>>,
        preloaded_pre: Option<CompactionPrereqs>,
    ) -> Result<CompactionStats, HarnessError> {
        let path = match preloaded_path {
            Some(p) => p,
            None => self.session.read_active_path().await?,
        };
        if path.is_empty() {
            return Err(CompactionError::InsufficientTokens.into());
        }

        let pre = preloaded_pre.unwrap_or_else(|| self.compaction_prereqs(&path));

        let abort = {
            let inner = self.inner.lock().unwrap();
            inner.current_abort.clone().unwrap_or_default()
        };

        // Capture query once to avoid redundant mutex lock+clone.
        let query = self.current_query_for_compaction();
        let query_ref = query.as_deref();

        // before_compact hook is called BEFORE prepare_compaction so that
        // BeforeCompactDecision::Override can bypass the token-threshold check entirely.
        if !self.hooks.before_compact.is_empty() {
            let composite = Arc::new(CompositeBeforeCompactHook::new(
                self.hooks.before_compact.clone(),
            ));
            let built = crate::session::session::build_context_from_entries(&path);
            let entry_ids: Vec<EntryId> = path.iter().map(|e| e.id).collect();
            let decision = composite
                .before_compact(BeforeCompactCtx {
                    estimated_tokens: pre.estimated_tokens,
                    messages: &built.messages,
                    entry_ids: &entry_ids,
                    context_window: pre.threshold_config.context_window,
                    reserve_tokens: pre.threshold_config.reserve_tokens,
                    keep_recent_tokens: pre.threshold_config.keep_recent_tokens,
                    current_query: query_ref,
                })
                .await;
            match decision {
                BeforeCompactDecision::Skip => {
                    return Err(CompactionError::Skipped.into());
                }
                BeforeCompactDecision::Override(result) => {
                    self.emit(AgentHarnessEvent::CompactionStart {
                        estimated_tokens: pre.estimated_tokens,
                    });
                    return self
                        .apply_compaction_result(result, vec![], &path, true)
                        .await;
                }
                BeforeCompactDecision::Compact => {
                    return self
                        .execute_compaction(&path, &pre, query_ref, &abort)
                        .await;
                }
                BeforeCompactDecision::Proceed => {}
            }
        }

        if pre.estimated_tokens <= pre.threshold {
            return Err(CompactionError::InsufficientTokens.into());
        }

        self.execute_compaction(&path, &pre, query_ref, &abort)
            .await
    }
    async fn execute_compaction(
        &self,
        path: &[SessionEntry],
        pre: &CompactionPrereqs,
        query: Option<&str>,
        abort: &CancellationToken,
    ) -> Result<CompactionStats, HarnessError> {
        let prep = compute_cut_point(
            path,
            pre.last_compaction.as_ref(),
            &pre.threshold_config,
            Some(pre.start_idx),
            Some(pre.estimated_tokens),
        )
        .ok_or(CompactionError::NoValidBoundary)?;
        let file_operations = prep.file_operations.clone();
        self.emit(AgentHarnessEvent::CompactionStart {
            estimated_tokens: prep.estimated_tokens,
        });
        let result = compact(
            pre.summary_client.as_ref(),
            prep,
            &pre.summary_config,
            query,
            abort,
        )
        .await?;
        self.apply_compaction_result(result, file_operations, path, false)
            .await
    }

    /// Compute shared compaction prerequisites from the session path and
    /// harness config. Used by both `try_auto_compact` (pre-check) and
    /// `do_compact` (execution) to avoid redundant computation.
    fn compaction_prereqs(&self, path: &[SessionEntry]) -> CompactionPrereqs {
        let last_compaction = path.iter().rev().find_map(|e| {
            if let SessionEntryPayload::Compaction(c) = &e.payload {
                Some(c.clone())
            } else {
                None
            }
        });

        let (threshold_config, summary_client, summary_config) = {
            let inner = self.inner.lock().unwrap();
            let context_window = inner
                .state
                .model_info
                .as_ref()
                .map(|mi| mi.context_window)
                .unwrap_or(DEFAULT_CONTEXT_WINDOW);

            let tc = CompactionThresholdConfig {
                reserve_tokens: inner.compaction_threshold.reserve_tokens,
                keep_recent_tokens: inner.compaction_threshold.keep_recent_tokens,
                context_window,
            };

            match &inner.compaction_summary {
                Some((client, cfg)) => (tc, client.clone(), cfg.clone()),
                None => {
                    let model = inner.state.model.clone();
                    let m_info = inner.state.model_info.clone().unwrap_or(ModelInfo {
                        context_window: DEFAULT_CONTEXT_WINDOW,
                        max_tokens: inner.max_tokens,
                    });
                    let sc = CompactionSummaryConfig {
                        summary_model: model,
                        summary_model_info: m_info,
                        prompt: inner.compaction_prompt.clone(),
                    };
                    (tc, self.client(), sc)
                }
            }
        };

        let start_idx = live_region_start(path);
        // `estimate_tokens` returns 0 for Compaction entries, but
        // `build_context_from_entries` injects the last compaction's summary
        // into the LLM context. Add it explicitly for accurate threshold check.
        let summary_tokens = last_compaction
            .as_ref()
            .map(estimate_summary_tokens)
            .unwrap_or(0);
        let estimated_tokens = estimate_tokens(&path[start_idx..]) + summary_tokens;
        let threshold = (threshold_config.context_window as usize)
            .saturating_sub(threshold_config.reserve_tokens);

        CompactionPrereqs {
            last_compaction,
            threshold_config,
            summary_config,
            summary_client,
            start_idx,
            estimated_tokens,
            threshold,
        }
    }

    async fn apply_compaction_result(
        &self,
        result: CompactionResult,
        file_operations: Vec<FileOperation>,
        path: &[SessionEntry],
        from_hook: bool,
    ) -> Result<CompactionStats, HarnessError> {
        let cut_idx = path
            .iter()
            .position(|e| e.id == result.first_kept_entry)
            .ok_or(CompactionError::InvalidFirstKeptEntry(
                result.first_kept_entry,
            ))?;
        let tokens_before = result.tokens_before;
        let tokens_after = result.tokens_after;
        // Only entries from the live region start onward are compressed
        // in this round; earlier entries were already compacted.
        let start_idx = live_region_start(path);
        let compressed_entries = cut_idx.saturating_sub(start_idx);

        let details = if file_operations.is_empty() {
            None
        } else {
            serde_json::to_value(&file_operations).ok()
        };

        let entry = CompactionEntry {
            summary_message: result.summary_message,
            first_kept_entry: result.first_kept_entry,
            tokens_before,
            from_hook,
            details,
        };
        self.session
            .append(SessionEntryPayload::Compaction(entry))
            .await?;

        Ok(CompactionStats {
            tokens_before,
            tokens_after,
            compressed_entries,
        })
    }
}
