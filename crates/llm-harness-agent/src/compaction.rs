use std::{fmt::Write as _, path::PathBuf};

use chrono::Utc;
use llm_harness_loop::LlmMessage;
use llm_harness_loop::{ChatRequest, LlmClient, RequestContent};
use llm_harness_types::*;

use crate::agent::ModelInfo;
use crate::session::types::{CompactionEntry, SessionEntry, SessionEntryPayload};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Tokens estimated per character (rough 4-char-per-token approximation).
const CHARS_PER_TOKEN: usize = 4;

/// System prompt used by the summary model during compaction.
const SUMMARIZATION_SYSTEM_PROMPT: &str = "\
You are a context summarization assistant. Your task is to read a conversation \
between a user and an AI coding assistant, then produce a structured summary \
that another LLM instance will use to continue the work seamlessly.\n\
\n\
Be concise but complete. Preserve:\n\
- What the user is trying to accomplish\n\
- What has been done and what remains\n\
- Key decisions and their rationale\n\
- Critical technical details (file paths, function names, error messages)\n\
- The immediate next steps\n";

/// User-facing instructions appended to each compaction summary request.
const SUMMARY_REQUEST: &str = "\
Create a structured context checkpoint summary using this format:\n\
\n\
## Goal\n\
[What is the user trying to accomplish?]\n\
\n\
## Progress\n\
### Done\n\
- [x] [Completed tasks]\n\
### In Progress\n\
- [ ] [Current work]\n\
\n\
## Key Decisions\n\
- **[Decision]**: [Brief rationale]\n\
\n\
## Next Steps\n\
1. [Ordered list]\n\
\n\
## Critical Context\n\
- [File paths, function names, error messages to preserve]\n";

// ── CompactionSettings ────────────────────────────────────────────────────────

/// Configuration for the compaction subsystem.
#[derive(Debug, Clone)]
pub struct CompactionSettings {
    /// Whether automatic compaction is enabled.
    pub enabled: bool,
    /// Tokens reserved for LLM response headroom; compaction triggers when
    /// `total_tokens > context_window - reserve_tokens`.
    pub reserve_tokens: usize,
    /// Tokens of recent history to preserve uncompressed.
    pub keep_recent_tokens: usize,
    /// Model ID used to generate the summary.
    pub summary_model: String,
    /// Metadata for the summary model (used for token budget checks).
    pub summary_model_info: ModelInfo,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
            summary_model: String::new(),
            summary_model_info: ModelInfo {
                context_window: 200_000,
                max_tokens: 8192,
            },
        }
    }
}

// ── CompactionPreparation ─────────────────────────────────────────────────────

/// Intermediate result of `prepare_compaction`: decision + cut-point data.
///
/// Separates the pure decision logic from the async LLM summarization call.
pub struct CompactionPreparation {
    /// Full path entries from root to active leaf.
    pub path_entries: Vec<SessionEntry>,
    /// First entry still valid from the previous compaction (or session root).
    pub first_kept_entry: EntryId,
    /// Entry at which the new compaction starts keeping content.
    pub cut_point: EntryId,
    /// Existing summary text (from last compaction) for iterative update mode.
    pub previous_summary: Option<String>,
    /// Estimated total token count before this compaction.
    pub estimated_tokens: usize,
    /// Entries that were split across the cut point (v1: always None).
    pub split_turn_prefix: Option<Vec<SessionEntry>>,
    /// File operations accumulated during the compacted period.
    pub file_operations: Vec<FileOperation>,
}

// ── Token estimation ──────────────────────────────────────────────────────────

fn estimate_tokens_for_text(s: &str) -> usize {
    (s.len() / CHARS_PER_TOKEN).max(1)
}

fn estimate_tokens_for_content_blocks(blocks: &[ContentBlock]) -> usize {
    blocks
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } => estimate_tokens_for_text(text),
            ContentBlock::Image { .. } => 250,
            ContentBlock::Thinking { thinking, .. } => estimate_tokens_for_text(thinking),
            ContentBlock::ToolUse { input, .. } => estimate_tokens_for_text(&input.to_string()),
        })
        .sum()
}

fn estimate_tokens_for_message(msg: &AgentMessage) -> usize {
    match msg {
        AgentMessage::User(u) => estimate_tokens_for_content_blocks(&u.content),
        AgentMessage::Assistant(a) => {
            // Use actual output token count when available; `input_tokens` reflects the
            // full request cost (all prior context), not the size of this message alone.
            if let Some(usage) = &a.usage {
                return usage.output_tokens as usize;
            }
            estimate_tokens_for_content_blocks(&a.content)
        }
        AgentMessage::ToolResult(t) => estimate_tokens_for_content_blocks(&t.content),
        AgentMessage::CompactionSummary(cs) => estimate_tokens_for_text(&cs.summary),
        AgentMessage::BranchSummary(bs) => estimate_tokens_for_text(&bs.summary),
        AgentMessage::Custom(c) => estimate_tokens_for_text(&c.data.to_string()),
    }
}

/// Estimates the token cost of a single session entry.
///
/// Uses actual usage data when available, falls back to character-count heuristics.
/// Non-message entries (config changes) contribute zero tokens.
pub(crate) fn estimate_tokens_for_entry(entry: &SessionEntry) -> usize {
    match &entry.payload {
        SessionEntryPayload::Message(msg) => estimate_tokens_for_message(msg),
        // Config changes contribute negligible tokens.
        _ => 0,
    }
}

// ── prepare_compaction ────────────────────────────────────────────────────────

/// Decide whether compaction is needed and determine the cut point.
///
/// Returns `None` when the current token count is within budget.
/// Pure function — does not call the LLM.
pub fn prepare_compaction(
    path: &[SessionEntry],
    last_compaction: Option<&CompactionEntry>,
    settings: &CompactionSettings,
    model_info: &ModelInfo,
) -> Option<CompactionPreparation> {
    if !settings.enabled || path.is_empty() {
        return None;
    }

    // Determine where the compactable region starts.
    let start_idx = last_compaction
        .and_then(|lc| path.iter().position(|e| e.id == lc.first_kept_entry))
        .unwrap_or(0);

    let compactable = &path[start_idx..];

    // Estimate total token cost.
    let estimated_tokens: usize = compactable.iter().map(estimate_tokens_for_entry).sum();

    let threshold = (model_info.context_window as usize).saturating_sub(settings.reserve_tokens);
    if estimated_tokens <= threshold {
        return None;
    }

    // Scan backward to find the first entry we need to keep.
    let mut kept_tokens: usize = 0;
    let mut cut_relative = compactable.len(); // index within compactable where kept section starts

    for (i, entry) in compactable.iter().enumerate().rev() {
        let t = estimate_tokens_for_entry(entry);
        if kept_tokens + t > settings.keep_recent_tokens && i + 1 < compactable.len() {
            cut_relative = i + 1;
            break;
        }
        kept_tokens += t;
        if i == 0 {
            // Even keeping everything doesn't exceed threshold; can't compact.
            return None;
        }
    }

    // Adjust cut_relative backward to land on a UserMessage boundary.
    let valid_cut = find_valid_cut_boundary(compactable, cut_relative);
    let valid_cut = valid_cut?; // None = no valid boundary found

    let first_kept_entry = compactable[valid_cut].id;
    let cut_point = first_kept_entry;
    let split_turn_prefix = (valid_cut < cut_relative)
        .then(|| compactable[valid_cut..cut_relative].to_vec())
        .filter(|entries| !entries.is_empty());

    // Extract previous summary from last compaction for iterative update.
    let previous_summary = last_compaction.and_then(|lc| {
        if let AgentMessage::CompactionSummary(cs) = &lc.summary_message {
            Some(cs.summary.clone())
        } else {
            None
        }
    });

    // The `first_kept_entry` field in the returned Preparation tracks what's
    // already been summarised (from the last compaction), not the new cut point.
    let prev_first_kept = last_compaction
        .map(|lc| lc.first_kept_entry)
        .unwrap_or(path[0].id);
    let file_operations = extract_file_operations(&compactable[..valid_cut]);

    Some(CompactionPreparation {
        path_entries: path.to_vec(),
        first_kept_entry: prev_first_kept,
        cut_point,
        previous_summary,
        estimated_tokens,
        split_turn_prefix,
        file_operations,
    })
}

/// Find the nearest index at or before `cut_relative` in `entries` that is a
/// valid turn boundary (a `UserMessage` or `CompactionSummary`).
///
/// Returns `None` if no valid boundary exists in the range.
fn find_valid_cut_boundary(entries: &[SessionEntry], cut_relative: usize) -> Option<usize> {
    // Scan backward from cut_relative (inclusive) to find a UserMessage.
    (0..=cut_relative.min(entries.len().saturating_sub(1)))
        .rev()
        .find(|&i| is_valid_cut_start(&entries[i]))
}

fn is_valid_cut_start(entry: &SessionEntry) -> bool {
    matches!(
        &entry.payload,
        SessionEntryPayload::Message(AgentMessage::User(_))
            | SessionEntryPayload::Message(AgentMessage::CompactionSummary(_))
            | SessionEntryPayload::Message(AgentMessage::BranchSummary(_))
            | SessionEntryPayload::Compaction(_)
    )
}

// ── compact ───────────────────────────────────────────────────────────────────

/// Call the summary LLM to generate a compaction summary.
///
/// Does **not** write to the session; the caller (Harness) appends the result.
/// If the auth hook is provided it is accepted but v1 ignores it — auth must be
/// configured at `client` construction time.
pub async fn compact(
    client: &(dyn LlmClient + Send + Sync),
    preparation: CompactionPreparation,
    settings: &CompactionSettings,
    _auth: Option<&(dyn AuthHook + Send + Sync)>,
) -> Result<CompactionResult, CompactionError> {
    // Find the cut point index in path_entries.
    let cut_idx = preparation
        .path_entries
        .iter()
        .position(|e| e.id == preparation.cut_point)
        .ok_or_else(|| {
            CompactionError::SummaryFailed(format!(
                "cut point {} not found in compaction path",
                preparation.cut_point
            ))
        })?;

    // For second+ compaction, only compress entries since the previous kept boundary,
    // not the entire path (those earlier entries are already captured in previous_summary).
    let first_kept_idx = preparation
        .path_entries
        .iter()
        .position(|e| e.id == preparation.first_kept_entry)
        .ok_or_else(|| {
            CompactionError::SummaryFailed(format!(
                "first kept entry {} not found in compaction path",
                preparation.first_kept_entry
            ))
        })?;

    let entries_to_compress = &preparation.path_entries[first_kept_idx..cut_idx];

    // Format conversation history as text.
    let conversation_text = format_entries_as_text(entries_to_compress);

    // Build user message.
    let user_content = if let Some(prev) = &preparation.previous_summary {
        format!(
            "The messages below are NEW conversation messages to incorporate into \
            the existing summary provided in <previous-summary> tags.\n\n\
            <previous-summary>\n{prev}\n</previous-summary>\n\n\
            New conversation:\n<conversation>\n{conversation_text}\n</conversation>\n\n\
            Update the summary to incorporate the new information. Preserve existing \
            completed items, update in-progress items, add new progress.\n\n{SUMMARY_REQUEST}"
        )
    } else {
        format!(
            "Here is the conversation to summarize:\n\n\
            <conversation>\n{conversation_text}\n</conversation>\n\n\
            {SUMMARY_REQUEST}"
        )
    };

    // Validate that the prompt fits within the summary model's context window.
    let prompt_tokens = estimate_tokens_for_text(SUMMARIZATION_SYSTEM_PROMPT)
        + estimate_tokens_for_text(&user_content);
    let max_input_tokens = (settings.summary_model_info.context_window as usize)
        .saturating_sub(settings.summary_model_info.max_tokens as usize);
    if prompt_tokens > max_input_tokens {
        return Err(CompactionError::SummaryFailed(format!(
            "conversation too long to summarize: ~{prompt_tokens} tokens exceeds \
             summary model input budget of {max_input_tokens}"
        )));
    }

    let req = ChatRequest::builder(
        settings.summary_model.clone(),
        settings.summary_model_info.max_tokens,
    )
    .messages(vec![
        LlmMessage::System(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        LlmMessage::User(vec![RequestContent::Text(user_content)]),
    ])
    .build();

    let response = client
        .chat(&req)
        .await
        .map_err(|e| CompactionError::SummaryFailed(e.to_string()))?;

    let summary_text = response.text();
    if summary_text.trim().is_empty() {
        return Err(CompactionError::SummaryFailed(
            "empty summary response".into(),
        ));
    }

    let split_note = preparation.split_turn_prefix.is_some().then(|| {
        "## Compaction Note\n\
         The cut point was moved backward to the nearest turn boundary; \
         extra entries before the requested token boundary were included in this \
         summary to preserve message ordering."
            .to_string()
    });

    // Append file operations summary if any.
    let full_summary = if preparation.file_operations.is_empty() {
        summary_text
    } else {
        let file_lines: Vec<String> = preparation
            .file_operations
            .iter()
            .map(|op| format!("- {:?}: {}", op.kind, op.path.display()))
            .collect();
        format!(
            "{summary_text}\n\n## Files Touched\n{}",
            file_lines.join("\n")
        )
    };
    let full_summary = if let Some(note) = split_note {
        format!("{full_summary}\n\n{note}")
    } else {
        full_summary
    };

    let summary_message = AgentMessage::CompactionSummary(CompactionSummaryMessage {
        summary: full_summary,
        timestamp: Utc::now(),
    });

    // Estimate tokens_after = summary + kept entries.
    let summary_tokens = estimate_tokens_for_message(&summary_message);
    let kept_tokens: usize = preparation.path_entries[cut_idx..]
        .iter()
        .map(estimate_tokens_for_entry)
        .sum();
    let tokens_after = summary_tokens + kept_tokens;

    Ok(CompactionResult {
        summary_message,
        first_kept_entry: preparation.cut_point,
        tokens_before: preparation.estimated_tokens,
        tokens_after,
        file_operations: preparation.file_operations,
    })
}

// ── Conversation serialization ────────────────────────────────────────────────

/// Serializes session entries to a human-readable string for the compaction LLM.
///
/// Only message and model-change entries are rendered; other entry types are skipped.
pub(crate) fn format_entries_as_text(entries: &[SessionEntry]) -> String {
    let mut out = String::new();

    for entry in entries {
        if !out.is_empty() {
            out.push_str("\n\n");
        }

        match &entry.payload {
            SessionEntryPayload::Message(msg) => {
                append_message_text(&mut out, msg);
            }
            SessionEntryPayload::ModelChange { to, .. } => {
                write!(&mut out, "[Config] Model changed to: {to}")
                    .expect("writing to String cannot fail");
            }
            _ => {
                let new_len = out.len().saturating_sub(2);
                out.truncate(new_len);
            }
        }
    }

    out
}

fn append_message_text(out: &mut String, msg: &AgentMessage) {
    match msg {
        AgentMessage::User(u) => {
            let text = content_blocks_to_text(&u.content);
            write!(out, "[User]\n{text}").expect("writing to String cannot fail");
        }
        AgentMessage::Assistant(a) => {
            let text = content_blocks_to_text(&a.content);
            write!(out, "[Assistant]\n{text}").expect("writing to String cannot fail");
        }
        AgentMessage::ToolResult(t) => {
            let text = content_blocks_to_text(&t.content);
            let status = if t.is_error { " (error)" } else { "" };
            write!(out, "[Tool Result{status}]\n{text}").expect("writing to String cannot fail");
        }
        AgentMessage::CompactionSummary(cs) => {
            write!(out, "[Previous Summary]\n{}", cs.summary)
                .expect("writing to String cannot fail");
        }
        AgentMessage::BranchSummary(bs) => {
            write!(out, "[Branch Summary]\n{}", bs.summary).expect("writing to String cannot fail");
        }
        AgentMessage::Custom(c) => {
            write!(out, "[Custom: {}]\n{}", c.r#type, c.data)
                .expect("writing to String cannot fail");
        }
    }
}

fn content_blocks_to_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } => text.clone(),
            ContentBlock::Thinking { thinking, .. } => thinking.clone(),
            ContentBlock::ToolUse { name, input, .. } => format!("[Tool: {name}({input})]"),
            ContentBlock::Image { .. } => "[image]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn extract_file_operations(entries: &[SessionEntry]) -> Vec<FileOperation> {
    let mut out = Vec::new();

    for entry in entries {
        match &entry.payload {
            SessionEntryPayload::Custom { data, .. } => {
                extract_file_operations_from_value(data, entry.id, &mut out);
            }
            SessionEntryPayload::Message(AgentMessage::Custom(custom)) => {
                extract_file_operations_from_value(&custom.data, entry.id, &mut out);
            }
            _ => {}
        }
    }

    out
}

fn extract_file_operations_from_value(
    value: &serde_json::Value,
    entry_id: EntryId,
    out: &mut Vec<FileOperation>,
) {
    if let Some(items) = value.get("file_operations").and_then(|v| v.as_array()) {
        for item in items {
            if let Some(op) = parse_file_operation(item, entry_id) {
                out.push(op);
            }
        }
        return;
    }

    if let Some(op) = parse_file_operation(value, entry_id) {
        out.push(op);
    }
}

fn parse_file_operation(value: &serde_json::Value, entry_id: EntryId) -> Option<FileOperation> {
    let path = value.get("path")?.as_str()?;
    let kind = value
        .get("kind")
        .and_then(|v| v.as_str())
        .and_then(parse_file_op_kind)?;

    Some(FileOperation {
        path: PathBuf::from(path),
        kind,
        at_entry: entry_id,
    })
}

fn parse_file_op_kind(kind: &str) -> Option<FileOpKind> {
    match kind {
        "read" | "Read" => Some(FileOpKind::Read),
        "modify" | "modified" | "write" | "Write" | "Modify" => Some(FileOpKind::Modify),
        _ => None,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "test-utils"))]
mod tests {
    use chrono::Utc;
    use llm_harness_loop::test_utils::MockLlmClient;
    use llm_harness_types::*;

    use crate::session::types::{SessionEntry, SessionEntryPayload};

    use super::*;

    fn user_entry(id: EntryId, text: &str) -> SessionEntry {
        SessionEntry {
            id,
            parent_id: None,
            timestamp: Utc::now(),
            payload: SessionEntryPayload::Message(AgentMessage::User(UserMessage {
                content: vec![ContentBlock::Text { text: text.into() }],
                timestamp: Utc::now(),
            })),
        }
    }

    fn assistant_entry(id: EntryId, text: &str, tokens: Option<TokenUsage>) -> SessionEntry {
        SessionEntry {
            id,
            parent_id: None,
            timestamp: Utc::now(),
            payload: SessionEntryPayload::Message(AgentMessage::Assistant(AssistantMessage {
                content: vec![ContentBlock::Text { text: text.into() }],
                stop_reason: Some(StopReason::EndTurn),
                timestamp: Utc::now(),
                provider: None,
                api: None,
                model: None,
                usage: tokens,
                error_message: None,
            })),
        }
    }

    fn custom_entry(id: EntryId, data: serde_json::Value) -> SessionEntry {
        SessionEntry {
            id,
            parent_id: None,
            timestamp: Utc::now(),
            payload: SessionEntryPayload::Custom {
                custom_type: "file_operations".into(),
                data,
            },
        }
    }

    fn model_change_entry(id: EntryId) -> SessionEntry {
        SessionEntry {
            id,
            parent_id: None,
            timestamp: Utc::now(),
            payload: SessionEntryPayload::ModelChange {
                to: "new-model".into(),
                provider: None,
                model_id: None,
            },
        }
    }

    fn compaction_entry(id: EntryId) -> SessionEntry {
        SessionEntry {
            id,
            parent_id: None,
            timestamp: Utc::now(),
            payload: SessionEntryPayload::Compaction(CompactionEntry {
                summary_message: AgentMessage::CompactionSummary(CompactionSummaryMessage {
                    summary: "summary".into(),
                    timestamp: Utc::now(),
                }),
                first_kept_entry: id,
                tokens_before: 100,
                from_hook: false,
                details: None,
            }),
        }
    }

    fn big_token_usage(n: u32) -> TokenUsage {
        TokenUsage {
            input_tokens: 0,
            output_tokens: n,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        }
    }

    fn settings() -> CompactionSettings {
        CompactionSettings {
            enabled: true,
            reserve_tokens: 1000,
            keep_recent_tokens: 100,
            summary_model: "test-model".into(),
            summary_model_info: ModelInfo {
                context_window: 2000,
                max_tokens: 512,
            },
        }
    }

    fn model_info() -> ModelInfo {
        ModelInfo {
            context_window: 2000,
            max_tokens: 512,
        }
    }

    #[test]
    fn no_compaction_when_disabled() {
        let settings = CompactionSettings {
            enabled: false,
            ..settings()
        };
        let path = vec![user_entry(EntryId::new(), "hello")];
        assert!(prepare_compaction(&path, None, &settings, &model_info()).is_none());
    }

    #[test]
    fn no_compaction_when_under_threshold() {
        // total tokens estimated from small messages ≪ threshold (2000 - 1000 = 1000)
        let path = vec![
            user_entry(EntryId::new(), "hi"),
            assistant_entry(EntryId::new(), "hello", None),
        ];
        assert!(prepare_compaction(&path, None, &settings(), &model_info()).is_none());
    }

    #[test]
    fn estimate_tokens_for_entry_counts_messages_only() {
        assert_eq!(
            estimate_tokens_for_entry(&model_change_entry(EntryId::new())),
            0
        );
        assert!(estimate_tokens_for_entry(&user_entry(EntryId::new(), "hello")) > 0);
    }

    #[test]
    fn compaction_entry_is_valid_cut_start() {
        assert!(is_valid_cut_start(&compaction_entry(EntryId::new())));
    }

    #[test]
    fn compaction_triggered_when_over_threshold() {
        // Force large token counts via usage field.
        let path = vec![
            user_entry(EntryId::new(), "a"),
            assistant_entry(EntryId::new(), "b", Some(big_token_usage(900))),
            user_entry(EntryId::new(), "c"),
            assistant_entry(EntryId::new(), "d", Some(big_token_usage(900))),
            user_entry(EntryId::new(), "recent question"),
        ];
        let prep = prepare_compaction(&path, None, &settings(), &model_info());
        assert!(
            prep.is_some(),
            "should trigger compaction when over threshold"
        );
    }

    #[test]
    fn prepare_compaction_extracts_file_operations_from_custom_entries() {
        let file_entry_id = EntryId::new();
        let path = vec![
            user_entry(EntryId::new(), "a"),
            custom_entry(
                file_entry_id,
                serde_json::json!({
                    "file_operations": [
                        { "path": "src/lib.rs", "kind": "read" },
                        { "path": "src/main.rs", "kind": "modify" }
                    ]
                }),
            ),
            assistant_entry(EntryId::new(), "b", Some(big_token_usage(1400))),
            user_entry(EntryId::new(), "recent"),
        ];

        let prep = prepare_compaction(&path, None, &settings(), &model_info()).unwrap();

        assert_eq!(prep.file_operations.len(), 2);
        assert_eq!(prep.file_operations[0].path, PathBuf::from("src/lib.rs"));
        assert!(matches!(prep.file_operations[0].kind, FileOpKind::Read));
        assert_eq!(prep.file_operations[0].at_entry, file_entry_id);
        assert_eq!(prep.file_operations[1].path, PathBuf::from("src/main.rs"));
        assert!(matches!(prep.file_operations[1].kind, FileOpKind::Modify));
    }

    #[test]
    fn prepare_compaction_marks_split_turn_boundary_downgrade() {
        let u1 = EntryId::new();
        let a1 = EntryId::new();
        let u2 = EntryId::new();
        let path = vec![
            user_entry(u1, "old question"),
            assistant_entry(a1, "large answer", Some(big_token_usage(900))),
            user_entry(u2, "recent question"),
            assistant_entry(EntryId::new(), "recent answer", Some(big_token_usage(900))),
        ];

        let prep = prepare_compaction(&path, None, &settings(), &model_info()).unwrap();

        assert_eq!(prep.cut_point, u2);
        let split = prep
            .split_turn_prefix
            .expect("boundary adjustment should be visible");
        assert_eq!(split[0].id, u2);
    }

    #[tokio::test]
    async fn compact_errors_when_first_kept_entry_is_missing() {
        let path = vec![
            user_entry(EntryId::new(), "old"),
            assistant_entry(EntryId::new(), "large", Some(big_token_usage(1400))),
            user_entry(EntryId::new(), "recent"),
        ];
        let cut_point = path[2].id;
        let client = MockLlmClient::new(vec![]);

        let err = compact(
            &client,
            CompactionPreparation {
                path_entries: path,
                first_kept_entry: EntryId::new(),
                cut_point,
                previous_summary: Some("previous".into()),
                estimated_tokens: 1400,
                split_turn_prefix: None,
                file_operations: vec![],
            },
            &settings(),
            None,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, CompactionError::SummaryFailed(message) if message.contains("first kept entry"))
        );
    }

    #[test]
    fn cut_point_lands_on_user_message() {
        let u1 = EntryId::new();
        let a1 = EntryId::new();
        let u2 = EntryId::new();
        let a2 = EntryId::new();
        let u3 = EntryId::new();
        let path = vec![
            user_entry(u1, "old question 1"),
            assistant_entry(a1, "answer 1", Some(big_token_usage(900))),
            user_entry(u2, "old question 2"),
            assistant_entry(a2, "answer 2", Some(big_token_usage(900))),
            user_entry(u3, "recent"),
        ];
        let prep = prepare_compaction(&path, None, &settings(), &model_info()).unwrap();
        // The cut_point should be a UserMessage entry.
        let cut_entry = path.iter().find(|e| e.id == prep.cut_point).unwrap();
        assert!(
            matches!(
                &cut_entry.payload,
                SessionEntryPayload::Message(AgentMessage::User(_))
            ),
            "cut point must be a user message"
        );
    }

    #[test]
    fn parse_command_args_basic() {
        use super::super::skills::parse_command_args;
        let args = parse_command_args("staging \"update API\" --dry-run");
        assert_eq!(args, vec!["staging", "update API", "--dry-run"]);
    }

    #[test]
    fn format_entries_as_text_includes_user_and_assistant() {
        let path = vec![
            user_entry(EntryId::new(), "hello"),
            assistant_entry(EntryId::new(), "world", None),
        ];
        let text = format_entries_as_text(&path);
        assert!(text.contains("[User]"));
        assert!(text.contains("hello"));
        assert!(text.contains("[Assistant]"));
        assert!(text.contains("world"));
    }
}
