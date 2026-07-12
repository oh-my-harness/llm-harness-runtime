// compaction_ops.rs — compaction methods extracted from harness/mod.rs
use std::sync::Arc;

use llm_harness_loop::composite::CompositeBeforeCompactHook;
use llm_harness_types::{
    BeforeCompactCtx, BeforeCompactDecision, BeforeCompactHook, CompactionError, CompactionResult,
    EntryId, HarnessError, HarnessPhase,
};
use tokio_util::sync::CancellationToken;

use crate::agent::ModelInfo;
use crate::compaction::{
    CompactionSettings, DEFAULT_CONTEXT_WINDOW, DEFAULT_KEEP_RECENT_TOKENS, DEFAULT_RESERVE_TOKENS,
    FileOperation, compact, compute_cut_point, estimate_summary_tokens, estimate_tokens,
};
use crate::session::types::{CompactionEntry, SessionEntry, SessionEntryPayload};

use super::AgentHarness;
use super::event::{AgentHarnessEvent, CompactionStats};
use super::phase_guard::AbortTokenGuard;

/// Find the index in `path` of the `first_kept_entry` belonging to the most
/// recent `Compaction` entry. Returns 0 when there is no prior compaction.
pub(super) fn live_region_start(path: &[SessionEntry]) -> usize {
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
///
/// `settings.summary_model` / `settings.summary_model_info` are placeholders
/// filled with the main conversation model's info — not the compaction model.
/// They are NOT safe to use for summary LLM calls. `do_compact` patches them
/// after resolving the summary client from `self.compaction_model`. This
/// indirection avoids cloning the summary client (`Arc<dyn LlmClient>`) during
/// the pre-check path where it is not needed. Only `settings.reserve_tokens`
/// and `settings.keep_recent_tokens` are valid in the pre-check path.
pub(super) struct CompactionPrereqs {
    pub(super) last_compaction: Option<CompactionEntry>,
    pub(super) m_info: ModelInfo,
    pub(super) settings: CompactionSettings,
    pub(super) start_idx: usize,
    pub(super) estimated_tokens: usize,
    pub(super) threshold: usize,
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
    pub async fn compact(&self) -> Result<CompactionStats, HarnessError> {
        self.compact_with_path(None).await
    }

    /// Internal compaction entry point that accepts an optional pre-loaded
    /// session path to avoid redundant `read_active_path` I/O when the caller
    /// already has the data (e.g. `try_auto_compact`).
    pub(super) async fn compact_with_path(
        &self,
        preloaded_path: Option<Vec<SessionEntry>>,
    ) -> Result<CompactionStats, HarnessError> {
        let _guard = self.enter_phase(HarnessPhase::Compacting)?;
        let result = self.do_compact(preloaded_path, None).await;
        match result {
            Ok(stats) => {
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
    pub(super) async fn try_auto_compact(&self) -> Result<(), HarnessError> {
        let (auto_compact, has_before_compact_hook) = {
            let inner = self.inner.lock().unwrap();
            (inner.auto_compact, !self.hooks.before_compact.is_empty())
        };
        if !auto_compact {
            return Ok(());
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
                &pre.settings,
                Some(pre.start_idx),
                Some(pre.estimated_tokens),
            )
            .is_none()
            {
                return self.handle_no_valid_boundary(
                    pre.estimated_tokens,
                    pre.m_info.context_window as usize,
                );
            }
        }

        // Save values needed for NoValidBoundary handling before `pre` is
        // moved into do_compact.
        let saved_estimated_tokens = pre.estimated_tokens;
        let saved_context_window = pre.m_info.context_window as usize;

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
                self.handle_no_valid_boundary(
                    saved_estimated_tokens,
                    saved_context_window,
                )
            }
            Err(HarnessError::Compaction(CompactionError::Skipped)) => Ok(()),
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

    pub(super) async fn do_compact(
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

        // Read config that is NOT in CompactionPrereqs: model, abort, summary client.
        let (model, abort) = {
            let inner = self.inner.lock().unwrap();
            (
                inner.state.model.clone(),
                inner.current_abort.clone().unwrap_or_default(),
            )
        };

        // Compaction uses the independent compaction config if set, otherwise
        // falls back to the main conversation client/model. The Option<struct>
        // makes half-configuration impossible at the type level.
        let (summary_client, summary_model, summary_model_info) = match &self.compaction_model {
            Some(c) => (c.client.clone(), c.model.clone(), c.model_info.clone()),
            None => (self.client.clone(), model.clone(), pre.m_info.clone()),
        };

        // Patch summary_model / summary_model_info into settings (prereqs used
        // placeholders since the summary client is only resolved here).
        let settings = CompactionSettings {
            summary_model,
            summary_model_info,
            ..pre.settings
        };

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
                    context_window: pre.m_info.context_window,
                    reserve_tokens: settings.reserve_tokens,
                    keep_recent_tokens: settings.keep_recent_tokens,
                })
                .await;
            match decision {
                BeforeCompactDecision::Skip => {
                    return Err(CompactionError::Skipped.into());
                }
                BeforeCompactDecision::Override(result) => {
                    return self
                        .apply_compaction_result(result, vec![], &path, true)
                        .await;
                }
                BeforeCompactDecision::Compact => {
                    let prep = compute_cut_point(
                        &path,
                        pre.last_compaction.as_ref(),
                        &settings,
                        Some(pre.start_idx),
                        Some(pre.estimated_tokens),
                    );
                    let prep = match prep {
                        Some(p) => p,
                        None => return Err(CompactionError::NoValidBoundary.into()),
                    };
                    let file_operations = prep.file_operations.clone();
                    self.emit(AgentHarnessEvent::CompactionStart {
                        estimated_tokens: prep.estimated_tokens,
                    });
                    let result = compact(summary_client.as_ref(), prep, &settings, &abort).await?;
                    return self
                        .apply_compaction_result(result, file_operations, &path, false)
                        .await;
                }
                BeforeCompactDecision::Proceed => {}
            }
        }

        if pre.estimated_tokens <= pre.threshold {
            return Err(CompactionError::InsufficientTokens.into());
        }

        let prep = compute_cut_point(
            &path,
            pre.last_compaction.as_ref(),
            &settings,
            Some(pre.start_idx),
            Some(pre.estimated_tokens),
        );
        let prep = match prep {
            Some(p) => p,
            None => return Err(CompactionError::NoValidBoundary.into()),
        };

        let estimated_tokens = prep.estimated_tokens;
        let file_operations = prep.file_operations.clone();
        self.emit(AgentHarnessEvent::CompactionStart { estimated_tokens });

        let result = compact(summary_client.as_ref(), prep, &settings, &abort).await?;
        self.apply_compaction_result(result, file_operations, &path, false)
            .await
    }

    /// Compute shared compaction prerequisites from the session path and
    /// harness config. Used by both `try_auto_compact` (pre-check) and
    /// `do_compact` (execution) to avoid redundant computation.
    ///
    /// `settings.summary_model` / `settings.summary_model_info` are
    /// placeholders — `do_compact` patches them after resolving the summary
    /// client.
    pub(super) fn compaction_prereqs(&self, path: &[SessionEntry]) -> CompactionPrereqs {
        let last_compaction = path.iter().rev().find_map(|e| {
            if let SessionEntryPayload::Compaction(c) = &e.payload {
                Some(c.clone())
            } else {
                None
            }
        });

        let (model_info, max_tokens, reserve_tokens, keep_recent_tokens) = {
            let inner = self.inner.lock().unwrap();
            (
                inner.state.model_info.clone(),
                inner.max_tokens,
                inner.compaction_reserve_tokens,
                inner.compaction_keep_recent_tokens,
            )
        };

        let m_info = model_info.unwrap_or(ModelInfo {
            context_window: DEFAULT_CONTEXT_WINDOW,
            max_tokens,
        });

        let settings = CompactionSettings {
            reserve_tokens: reserve_tokens.unwrap_or(DEFAULT_RESERVE_TOKENS as u32) as usize,
            keep_recent_tokens: keep_recent_tokens.unwrap_or(DEFAULT_KEEP_RECENT_TOKENS as u32)
                as usize,
            summary_model: String::new(),
            summary_model_info: m_info.clone(),
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
        let threshold = (m_info.context_window as usize).saturating_sub(settings.reserve_tokens);

        CompactionPrereqs {
            last_compaction,
            m_info,
            settings,
            start_idx,
            estimated_tokens,
            threshold,
        }
    }

    pub(super) async fn apply_compaction_result(
        &self,
        result: CompactionResult,
        file_operations: Vec<FileOperation>,
        path: &[SessionEntry],
        from_hook: bool,
    ) -> Result<CompactionStats, HarnessError> {
        if !path.iter().any(|e| e.id == result.first_kept_entry) {
            return Err(CompactionError::InvalidFirstKeptEntry(result.first_kept_entry).into());
        }

        let tokens_before = result.tokens_before;
        let tokens_after = result.tokens_after;
        let cut_idx = path
            .iter()
            .position(|e| e.id == result.first_kept_entry)
            .expect("first_kept_entry validated before apply_compaction_result");
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
