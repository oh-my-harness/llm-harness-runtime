use std::{fmt::Write as _, path::PathBuf, sync::Arc};

use chrono::Utc;
use llm_harness_loop::LlmMessage;
use llm_harness_loop::{ChatRequest, LlmClient, RequestContent};
use llm_harness_types::*;
use serde::{Deserialize, Serialize, de};
use tokio_util::sync::CancellationToken;

use crate::agent::ModelInfo;
use crate::session::types::{CompactionEntry, CompactionPolicy, SessionEntry, SessionEntryPayload};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Bytes per token for the rough character-count heuristic (≈4 bytes/token for
/// ASCII; CJK text is over-estimated since UTF-8 uses 3 bytes/char).
const CHARS_PER_TOKEN: usize = 4;
/// Fixed token estimate for an image content block (images aren't tokenized by char count).
const IMAGE_TOKEN_ESTIMATE: usize = 250;

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
- The immediate next steps\n\
\n";

/// Instruction appended to every compaction system prompt (both hardcoded
/// and custom) so the LLM knows to preserve <no-compact> tagged content.
const NO_COMPACT_INSTRUCTION: &str = "\
Content wrapped in <no-compact> tags must be preserved verbatim in your summary — do not summarize, paraphrase, or omit it.\n";

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

// ── Compaction config ─────────────────────────────────────────────────────────

/// Whether a file was read or modified during the compacted period.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FileOpKind {
    /// File was read.
    Read,
    /// File was created or modified.
    Modify,
}

/// A file operation recorded for inclusion in the compaction summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileOperation {
    /// Absolute path of the file.
    pub path: PathBuf,
    /// Whether the file was read or modified.
    pub kind: FileOpKind,
    /// Session entry ID at which this operation occurred.
    pub at_entry: EntryId,
}

/// 阈值与切点配置。用于 pre-check 和切点计算。
#[derive(Debug, Clone)]
pub struct CompactionThresholdConfig {
    /// 为 LLM 响应预留的 token 数。
    pub reserve_tokens: usize,
    /// 保留不压缩的近期历史 token 数。
    pub keep_recent_tokens: usize,
    /// 主对话模型的上下文窗口大小（决定何时触发压缩）。
    /// 注意：不是 summary model 的 context_window。
    pub context_window: u32,
}

/// Custom compaction prompt configuration.
///
/// When set on [`CompactionSummaryConfig`], replaces the hardcoded
/// `SUMMARIZATION_SYSTEM_PROMPT` / `SUMMARY_REQUEST` constants.
///
/// `user_template` supports these placeholders:
/// - `{conversation}` — text-serialized entries to compress
/// - `{previous_summary}` — prior compaction summary text (if any)
/// - `{file_operations}` — formatted file operation list (if any)
/// - `{query}` — current task query intent (if provided by caller/hook)
///
/// `{` and `}` are reserved for placeholder syntax — there is no escape
/// sequence (e.g. `{{`). Templates must not contain literal braces outside
/// of recognized placeholders; `new()` will reject unknown `{...}` patterns.
/// If the prompt needs literal JSON or other braced text, put it in the
/// `system_prompt` (which is not parsed for placeholders).
#[derive(Debug, Clone, Serialize)]
pub struct CompactionPromptSpec {
    /// System prompt for the summary LLM.
    pub(crate) system_prompt: String,
    /// User message template with placeholder substitution.
    pub(crate) user_template: String,
}

/// Known placeholders supported by `CompactionPromptSpec::user_template`.
const KNOWN_PLACEHOLDERS: &[&str] = &[
    "conversation",
    "previous_summary",
    "file_operations",
    "query",
];

/// Find the closest known placeholder to `name` by edit distance.
fn closest_placeholder(name: &str) -> String {
    KNOWN_PLACEHOLDERS
        .iter()
        .map(|&candidate| (candidate, edit_distance(name, candidate)))
        .min_by_key(|&(_, d)| d)
        .map(|(candidate, _)| candidate.to_string())
        .expect("KNOWN_PLACEHOLDERS is non-empty")
}

/// Simple Levenshtein edit distance.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0usize; n + 1];
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

impl CompactionPromptSpec {
    /// Create a new prompt spec, validating that `user_template` only
    /// contains known placeholders and includes `{conversation}`.
    ///
    /// Returns `Err(UnknownPlaceholder)` if an unrecognized `{...}` pattern
    /// is found, with a suggestion for the closest known placeholder.
    ///
    /// Returns `Err(MissingRequiredPlaceholder)` if `{conversation}` is
    /// absent — without it the summary LLM receives no conversation content.
    pub fn new(
        system_prompt: impl Into<String>,
        user_template: impl Into<String>,
    ) -> Result<Self, llm_harness_types::TemplateError> {
        let user_template = user_template.into();
        let mut start = 0;
        while let Some(open) = user_template[start..].find('{') {
            let abs_open = start + open;
            if let Some(close) = user_template[abs_open + 1..].find('}') {
                let abs_close = abs_open + 1 + close;
                let name = &user_template[abs_open + 1..abs_close];
                if !name.is_empty() && !KNOWN_PLACEHOLDERS.contains(&name) {
                    let suggestion = closest_placeholder(name);
                    return Err(llm_harness_types::TemplateError::UnknownPlaceholder {
                        found: name.to_string(),
                        suggestion,
                    });
                }
                start = abs_close + 1;
            } else {
                break;
            }
        }
        if !user_template.contains("{conversation}") {
            return Err(
                llm_harness_types::TemplateError::MissingRequiredPlaceholder("conversation".into()),
            );
        }
        Ok(Self {
            system_prompt: system_prompt.into(),
            user_template,
        })
    }
}

impl CompactionPromptSpec {
    /// Returns the system prompt.
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Returns the user message template.
    pub fn user_template(&self) -> &str {
        &self.user_template
    }
}

impl<'de> Deserialize<'de> for CompactionPromptSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            system_prompt: String,
            user_template: String,
        }
        let raw = Raw::deserialize(deserializer)?;
        CompactionPromptSpec::new(raw.system_prompt, raw.user_template).map_err(de::Error::custom)
    }
}

/// Render a template by scanning it once and replacing `{placeholder}` patterns
/// with the corresponding value. Inserted values are never re-scanned, preventing
/// prompt injection via placeholder-like content in user/tool messages.
fn render_template(template: &str, replacements: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        if let Some(close) = after.find('}') {
            let name = &after[..close];
            if let Some((_, value)) = replacements.iter().find(|(key, _)| *key == name) {
                out.push_str(value);
            } else {
                // Unknown placeholder — pass through verbatim.
                out.push('{');
                out.push_str(name);
                out.push('}');
            }
            rest = &after[close + 1..];
        } else {
            // No closing brace — pass through the rest verbatim.
            out.push('{');
            out.push_str(after);
            break;
        }
    }
    out.push_str(rest);
    out
}

/// Summary LLM 调用配置。
#[derive(Debug, Clone)]
pub struct CompactionSummaryConfig {
    /// 生成摘要的模型 ID。
    pub summary_model: String,
    /// Summary 模型的元数据（上下文窗口、最大输出 token 数）。
    pub summary_model_info: ModelInfo,
    /// Custom prompt spec. `None` → use hardcoded constants.
    pub prompt: Option<CompactionPromptSpec>,
}

impl CompactionThresholdConfig {
    /// 使用默认值构造。
    pub fn with_defaults(context_window: u32) -> Self {
        Self {
            reserve_tokens: DEFAULT_RESERVE_TOKENS,
            keep_recent_tokens: DEFAULT_KEEP_RECENT_TOKENS,
            context_window,
        }
    }
}

/// Default `reserve_tokens` used when `compaction_reserve_tokens` is `None`.
pub(crate) const DEFAULT_RESERVE_TOKENS: usize = 16_384;
/// Default `keep_recent_tokens` used when `compaction_keep_recent_tokens` is `None`.
pub(crate) const DEFAULT_KEEP_RECENT_TOKENS: usize = 20_000;
/// Default `context_window` used when `model_info` is `None`.
pub(crate) const DEFAULT_CONTEXT_WINDOW: u32 = 200_000;

/// Independent LLM provider/model for compaction summaries.
///
/// When set on [`crate::AgentHarnessOptions`], compaction uses this provider
/// instead of the main conversation client — e.g. run the main conversation
/// on an expensive model while compaction uses a cheap one.
///
/// All three fields are bound: they describe one provider link and must be
/// set together. Encapsulating them in a struct makes half-configuration
/// impossible at the type level.
pub struct CompactionModelConfig {
    /// LLM client (already bound to a provider/credential at construction).
    pub client: Arc<dyn LlmClient>,
    /// Model ID the client routes.
    pub model: String,
    /// Model metadata (context_window / max_tokens) for prompt budget checks.
    pub model_info: ModelInfo,
}

/// Intermediate result of `compute_cut_point`: decision + cut-point data.
///
/// Separates the pure decision logic from the async LLM summarization call.
pub struct CompactionPreparation {
    /// Entries to compress into the summary (from compress_from to cut_point).
    pub entries_to_compress: Vec<SessionEntry>,
    /// Entries kept after compaction (from cut_point to end of path).
    pub kept_entries: Vec<SessionEntry>,
    /// 本次压缩起点（上次 compaction 的保留区起点）。
    pub compress_from_entry: EntryId,
    /// Entry at which the new compaction starts keeping content.
    pub cut_point: EntryId,
    /// Existing summary text (from last compaction) for iterative update mode.
    pub previous_summary: Option<String>,
    /// Estimated total token count before this compaction.
    pub estimated_tokens: usize,
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
            ContentBlock::Image { .. } => IMAGE_TOKEN_ESTIMATE,
            ContentBlock::Thinking { thinking, .. } => estimate_tokens_for_text(thinking),
            ContentBlock::ToolUse { input, .. } => estimate_tokens_for_text(&input.to_string()),
        })
        .sum()
}

fn estimate_tokens_for_data_blocks(blocks: &[DataBlock]) -> usize {
    blocks
        .iter()
        .map(|block| match block {
            DataBlock::Text { text, .. } => estimate_tokens_for_text(text),
            DataBlock::Image { .. } => IMAGE_TOKEN_ESTIMATE,
            DataBlock::Document { data, .. } => match data {
                BinarySource::Base64 { data } => (data.len() / CHARS_PER_TOKEN).max(1),
                BinarySource::Blob { .. } => 1,
            },
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
                return (usage.output_tokens + usage.reasoning_tokens) as usize;
            }
            estimate_tokens_for_content_blocks(&a.content)
        }
        AgentMessage::ToolResult(t) => estimate_tokens_for_data_blocks(&t.content),
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

/// Estimate the token cost of a compaction entry's summary message.
///
/// `build_context_from_entries` injects this summary into the LLM context
/// (it is not part of `estimate_tokens_for_entry`, which returns 0 for
/// `Compaction` entries). Callers that need the *effective* context size
/// must add this explicitly.
pub(crate) fn estimate_summary_tokens(entry: &CompactionEntry) -> usize {
    estimate_tokens_for_message(&entry.summary_message)
}
/// Estimate total token count for a session path.
pub(crate) fn estimate_tokens(path: &[SessionEntry]) -> usize {
    path.iter().map(estimate_tokens_for_entry).sum()
}

// ── prepare_compaction ────────────────────────────────────────────────────────

/// Compute cut point for compaction without threshold check.
///
/// Returns `None` when the compactable region is too short to compress
/// (total tokens < keep_recent_tokens, or no valid cut boundary found).
/// Callers should map `None` to [`CompactionError::NoValidBoundary`].
///
/// If `start_idx` and `estimated_tokens` are already known (e.g. computed by
/// `compute_cut_point` or `try_auto_compact`), pass them in to avoid redundant O(n) scans.
pub(crate) fn compute_cut_point(
    path: &[SessionEntry],
    last_compaction: Option<&CompactionEntry>,
    threshold: &CompactionThresholdConfig,
    start_idx: Option<usize>,
    estimated_tokens: Option<usize>,
) -> Option<CompactionPreparation> {
    if path.is_empty() {
        return None;
    }

    let start_idx = start_idx.unwrap_or_else(|| {
        last_compaction
            .and_then(|lc| path.iter().position(|e| e.id == lc.first_kept_entry))
            .unwrap_or(0)
    });

    let compactable = &path[start_idx..];
    if compactable.is_empty() {
        return None;
    }

    // Scan backward to find the first entry we need to keep.
    let mut kept_tokens: usize = 0;
    let mut cut_relative = compactable.len();

    for (i, entry) in compactable.iter().enumerate().rev() {
        let t = estimate_tokens_for_entry(entry);
        if kept_tokens + t > threshold.keep_recent_tokens && i + 1 < compactable.len() {
            cut_relative = i + 1;
            break;
        }
        kept_tokens += t;
        if i == 0 {
            return None;
        }
    }

    let valid_cut = find_valid_cut_boundary(compactable, cut_relative)?;
    // If the cut point is the same as the compress-from entry, the range
    // is empty — nothing to compact.
    let first_kept_entry = compactable[valid_cut].id;
    let cut_point = first_kept_entry;
    let prev_first_kept = last_compaction
        .map(|lc| lc.first_kept_entry)
        .unwrap_or(path[0].id);
    if cut_point == prev_first_kept {
        return None;
    }
    let previous_summary = last_compaction.and_then(|lc| {
        if let AgentMessage::CompactionSummary(cs) = &lc.summary_message {
            Some(cs.summary.clone())
        } else {
            None
        }
    });

    let file_operations = extract_file_operations(&compactable[..valid_cut]);

    let cut_idx = start_idx + valid_cut;
    let entries_to_compress = path[start_idx..cut_idx].to_vec();
    let kept_entries = path[cut_idx..].to_vec();

    Some(CompactionPreparation {
        entries_to_compress,
        kept_entries,
        compress_from_entry: prev_first_kept,
        cut_point,
        previous_summary,
        estimated_tokens: estimated_tokens.unwrap_or_else(|| {
            let summary_tokens = last_compaction.map(estimate_summary_tokens).unwrap_or(0);
            estimate_tokens(compactable) + summary_tokens
        }),
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
pub async fn compact(
    client: &(dyn LlmClient + Send + Sync),
    preparation: CompactionPreparation,
    summary: &CompactionSummaryConfig,
    query: Option<&str>,
    abort: &CancellationToken,
) -> Result<CompactionResult, CompactionError> {
    if preparation.entries_to_compress.is_empty() {
        return Err(CompactionError::SummaryFailed(
            "nothing to compact: compress range is empty".into(),
        ));
    }

    // Format conversation history as text.
    let conversation_text = format_entries_as_text(&preparation.entries_to_compress);

    if conversation_text.trim().is_empty() {
        return Err(CompactionError::SummaryFailed(
            "nothing to compact: conversation text is empty after filtering".into(),
        ));
    }

    // Build system + user prompt. When a custom prompt spec is configured,
    // render the template with placeholder substitution; otherwise fall back
    // to the hardcoded constants.
    let (system_prompt_str, user_content) = match &summary.prompt {
        None => {
            // When a query is provided, prepend it as task context so the
            // summary LLM can prioritize task-relevant information.
            let query_prefix = match query {
                Some(q) if !q.is_empty() => {
                    format!("Task context: {q}\n\n")
                }
                _ => String::new(),
            };
            let uc = if let Some(prev) = &preparation.previous_summary {
                format!(
                    "{query_prefix}The messages below are NEW conversation messages to \
                    incorporate into the existing summary provided in <previous-summary> \
                    tags.\n\n\
                    <previous-summary>\n{prev}\n</previous-summary>\n\n\
                    New conversation:\n<conversation>\n{conversation_text}\n</conversation>\n\n\
                    Update the summary to incorporate the new information. Preserve existing \
                    completed items, update in-progress items, add new progress.\n\n{SUMMARY_REQUEST}"
                )
            } else {
                format!(
                    "{query_prefix}Here is the conversation to summarize:\n\n\
                    <conversation>\n{conversation_text}\n</conversation>\n\n\
                    {SUMMARY_REQUEST}"
                )
            };
            (
                format!("{SUMMARIZATION_SYSTEM_PROMPT}\n{NO_COMPACT_INSTRUCTION}"),
                uc,
            )
        }
        Some(spec) => {
            let prev = preparation.previous_summary.as_deref().unwrap_or("");
            let file_ops = if preparation.file_operations.is_empty() {
                String::new()
            } else {
                preparation
                    .file_operations
                    .iter()
                    .map(|op| format!("- {:?}: {}", op.kind, op.path.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let q = query.unwrap_or("");
            let uc = render_template(
                &spec.user_template,
                &[
                    ("conversation", &conversation_text),
                    ("previous_summary", prev),
                    ("file_operations", &file_ops),
                    ("query", q),
                ],
            );
            (
                format!("{}\n{}", spec.system_prompt, NO_COMPACT_INSTRUCTION),
                uc,
            )
        }
    };

    // Validate that the prompt fits within the summary model's context window.
    let prompt_tokens =
        estimate_tokens_for_text(&system_prompt_str) + estimate_tokens_for_text(&user_content);
    let max_input_tokens = (summary.summary_model_info.context_window as usize)
        .saturating_sub(summary.summary_model_info.max_tokens as usize);
    if prompt_tokens > max_input_tokens {
        return Err(CompactionError::SummaryFailed(format!(
            "conversation too long to summarize: ~{prompt_tokens} tokens exceeds \
             summary model input budget of {max_input_tokens}"
        )));
    }

    let req = ChatRequest::builder(
        summary.summary_model.clone(),
        summary.summary_model_info.max_tokens,
    )
    .messages(vec![
        LlmMessage::System(system_prompt_str),
        LlmMessage::User(vec![RequestContent::Text(user_content)]),
    ])
    .build();

    let response = tokio::select! {
        biased;
        _ = abort.cancelled() => {
            return Err(CompactionError::Agent(AgentError::Aborted));
        }
        r = client.chat(&req) => {
            r.map_err(|e| CompactionError::SummaryFailed(e.to_string()))?
        }
    };

    let summary_text = response.text();
    if summary_text.trim().is_empty() {
        return Err(CompactionError::SummaryFailed(
            "empty summary response".into(),
        ));
    }

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

    // Structurally preserve Keep-marked entries: append their rendered text
    // verbatim to the summary. This is a hard guarantee — even if the LLM
    // ignores the <no-compact> instruction in the prompt, the content is
    // still preserved in the summary message.
    let kept_verbatim: Vec<String> = preparation
        .entries_to_compress
        .iter()
        .filter(|e| e.compaction_policy == Some(CompactionPolicy::Keep))
        .map(|e| {
            let mut buf = String::new();
            match &e.payload {
                SessionEntryPayload::Message(msg) => append_message_text(&mut buf, msg),
                SessionEntryPayload::ModelChange { to, .. } => {
                    write!(buf, "[Config] Model changed to: {to}").unwrap();
                }
                _ => {}
            }
            buf
        })
        .filter(|s| !s.trim().is_empty())
        .collect();
    let full_summary = if kept_verbatim.is_empty() {
        full_summary
    } else {
        format!(
            "{full_summary}\n\n## Preserved Content (verbatim)\n{}",
            kept_verbatim.join("\n\n")
        )
    };

    let summary_message = AgentMessage::CompactionSummary(CompactionSummaryMessage {
        summary: full_summary,
        timestamp: Utc::now(),
    });

    // Estimate tokens_after = summary + kept entries.
    let summary_tokens = estimate_tokens_for_message(&summary_message);
    let kept_tokens: usize = preparation
        .kept_entries
        .iter()
        .map(estimate_tokens_for_entry)
        .sum();
    let tokens_after = summary_tokens + kept_tokens;

    Ok(CompactionResult {
        summary_message,
        first_kept_entry: preparation.cut_point,
        tokens_before: preparation.estimated_tokens,
        tokens_after,
    })
}

// ── Conversation serialization ────────────────────────────────────────────────

/// Serializes session entries to a human-readable string for the compaction LLM.
///
/// Only message and model-change entries are rendered; other entry types are skipped.
pub(crate) fn format_entries_as_text(entries: &[SessionEntry]) -> String {
    let mut out = String::new();

    for entry in entries {
        let rendered = match &entry.payload {
            SessionEntryPayload::Message(msg) => {
                let mut buf = String::new();
                append_message_text(&mut buf, msg);
                buf
            }
            SessionEntryPayload::ModelChange { to, .. } => {
                format!("[Config] Model changed to: {to}")
            }
            _ => continue,
        };
        let rendered = if entry.compaction_policy == Some(CompactionPolicy::Keep) {
            // Escape no-compact tags in content to prevent premature
            // termination or spurious nesting that would confuse the LLM.
            let escaped = rendered
                .replace("</no-compact>", "&lt;/no-compact&gt;")
                .replace("<no-compact", "&lt;no-compact");
            format!("<no-compact preserve=\"verbatim\">\n{escaped}\n</no-compact>")
        } else {
            rendered
        };
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&rendered);
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
            let text = data_blocks_to_text(&t.content);
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

fn data_blocks_to_text(blocks: &[DataBlock]) -> String {
    blocks
        .iter()
        .map(|block| match block {
            DataBlock::Text { text, .. } => text.clone(),
            DataBlock::Image { .. } => "[image]".to_string(),
            DataBlock::Document {
                name, media_type, ..
            } => format!(
                "[document: {} ({media_type})]",
                name.as_deref().unwrap_or("unnamed")
            ),
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
    if kind.eq_ignore_ascii_case("read") {
        Some(FileOpKind::Read)
    } else if kind.eq_ignore_ascii_case("modify")
        || kind.eq_ignore_ascii_case("modified")
        || kind.eq_ignore_ascii_case("write")
        || kind.eq_ignore_ascii_case("created")
    {
        Some(FileOpKind::Modify)
    } else {
        None
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
            compaction_policy: None,
        }
    }

    fn assistant_entry(id: EntryId, text: &str, tokens: Option<TokenUsage>) -> SessionEntry {
        SessionEntry {
            id,
            parent_id: None,
            timestamp: Utc::now(),
            payload: SessionEntryPayload::Message(AgentMessage::Assistant({
                let mut m = llm_harness_loop::test_utils::test_assistant_message(vec![
                    ContentBlock::Text { text: text.into() },
                ]);
                m.stop_reason = Some(StopReason::EndTurn);
                m.usage = tokens;
                m
            })),
            compaction_policy: None,
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
            compaction_policy: None,
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
            compaction_policy: None,
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
            compaction_policy: None,
        }
    }

    fn big_token_usage(n: u32) -> TokenUsage {
        TokenUsage {
            input_tokens: 0,
            output_tokens: n,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            reasoning_tokens: 0,
            provenance: UsageReportProvenance::ReportedValid,
        }
    }

    fn settings() -> CompactionThresholdConfig {
        CompactionThresholdConfig {
            reserve_tokens: 1000,
            keep_recent_tokens: 100,
            context_window: 2000,
        }
    }

    fn summary_config() -> CompactionSummaryConfig {
        CompactionSummaryConfig {
            summary_model: "test-model".into(),
            summary_model_info: ModelInfo {
                context_window: 2000,
                max_tokens: 512,
            },
            prompt: None,
        }
    }

    fn model_info() -> ModelInfo {
        ModelInfo {
            context_window: 2000,
            max_tokens: 512,
        }
    }
    fn prepare_compaction(
        path: &[SessionEntry],
        last_compaction: Option<&CompactionEntry>,
        threshold: &CompactionThresholdConfig,
        model_info: &ModelInfo,
    ) -> Option<CompactionPreparation> {
        if path.is_empty() {
            return None;
        }

        let start_idx = last_compaction
            .and_then(|lc| path.iter().position(|e| e.id == lc.first_kept_entry))
            .unwrap_or(0);

        let compactable = &path[start_idx..];
        let summary_tokens = last_compaction.map(estimate_summary_tokens).unwrap_or(0);
        let estimated_tokens: usize = estimate_tokens(compactable) + summary_tokens;

        let cut_threshold =
            (model_info.context_window as usize).saturating_sub(threshold.reserve_tokens);
        if estimated_tokens <= cut_threshold {
            return None;
        }

        compute_cut_point(
            path,
            last_compaction,
            threshold,
            Some(start_idx),
            Some(estimated_tokens),
        )
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
    fn prepare_compaction_cut_point_adjusted_to_turn_boundary() {
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

    #[test]
    fn format_entries_wraps_keep_entries_in_no_compact_tag() {
        use crate::session::types::{SessionEntry, SessionEntryPayload};
        use chrono::Utc;

        let normal = SessionEntry {
            id: EntryId::new(),
            parent_id: None,
            timestamp: Utc::now(),
            payload: SessionEntryPayload::Message(AgentMessage::User(UserMessage {
                content: vec![ContentBlock::Text {
                    text: "normal message".into(),
                }],
                timestamp: Utc::now(),
            })),
            compaction_policy: None,
        };
        let keep = SessionEntry {
            id: EntryId::new(),
            parent_id: None,
            timestamp: Utc::now(),
            payload: SessionEntryPayload::Message(AgentMessage::User(UserMessage {
                content: vec![ContentBlock::Text {
                    text: "important decision".into(),
                }],
                timestamp: Utc::now(),
            })),
            compaction_policy: Some(CompactionPolicy::Keep),
        };

        let text = format_entries_as_text(&[normal, keep]);

        assert!(text.contains("normal message"), "normal entry text present");
        assert!(
            text.contains("<no-compact"),
            "keep entry must be wrapped in no-compact tag"
        );
        assert!(
            text.contains("important decision"),
            "keep entry text present"
        );
        assert!(
            text.contains("</no-compact>"),
            "no-compact tag must be closed"
        );
        // The normal entry should not itself be wrapped.
        let normal_segment = text.split("<no-compact").next().unwrap();
        assert!(
            normal_segment.contains("normal message"),
            "normal entry must not be wrapped in no-compact tag"
        );
    }

    #[test]
    fn file_op_kind_serde_roundtrip() {
        let k = FileOpKind::Modify;
        let json = serde_json::to_string(&k).unwrap();
        let k2: FileOpKind = serde_json::from_str(&json).unwrap();
        assert!(matches!(k2, FileOpKind::Modify));
    }

    #[test]
    fn file_operation_fields() {
        let op = FileOperation {
            path: PathBuf::from("/foo/bar.rs"),
            kind: FileOpKind::Read,
            at_entry: EntryId::new(),
        };
        assert!(op.path.ends_with("bar.rs"));
        assert!(matches!(op.kind, FileOpKind::Read));
    }

    #[tokio::test]
    async fn compact_returns_error_when_nothing_to_compress() {
        // Empty entries_to_compress → "nothing to compact" error.
        let entry = user_entry(EntryId::new(), "only entry");
        let client = MockLlmClient::new(vec![]);

        let err = compact(
            &client,
            CompactionPreparation {
                entries_to_compress: vec![],
                kept_entries: vec![entry.clone()],
                compress_from_entry: entry.id,
                cut_point: entry.id,
                previous_summary: None,
                estimated_tokens: 0,
                file_operations: vec![],
            },
            &summary_config(),
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, CompactionError::SummaryFailed(ref msg) if msg.contains("nothing to compact")),
            "expected nothing-to-compact error, got: {err:?}"
        );
    }

    #[test]
    fn parse_file_op_kind_case_insensitive_and_created() {
        assert!(matches!(parse_file_op_kind("read"), Some(FileOpKind::Read)));
        assert!(matches!(parse_file_op_kind("READ"), Some(FileOpKind::Read)));
        assert!(matches!(parse_file_op_kind("Read"), Some(FileOpKind::Read)));
        assert!(matches!(
            parse_file_op_kind("modify"),
            Some(FileOpKind::Modify)
        ));
        assert!(matches!(
            parse_file_op_kind("Modified"),
            Some(FileOpKind::Modify)
        ));
        assert!(matches!(
            parse_file_op_kind("Write"),
            Some(FileOpKind::Modify)
        ));
        assert!(matches!(
            parse_file_op_kind("created"),
            Some(FileOpKind::Modify)
        ));
        assert!(matches!(
            parse_file_op_kind("CREATED"),
            Some(FileOpKind::Modify)
        ));
        assert!(parse_file_op_kind("unknown").is_none());
    }
    #[test]
    fn estimate_tokens_returns_zero_for_empty_path() {
        let path: Vec<SessionEntry> = vec![];
        assert_eq!(estimate_tokens(&path), 0);
    }

    #[test]
    fn estimate_tokens_sums_entries() {
        let path = vec![
            user_entry(EntryId::new(), "hello"),
            assistant_entry(EntryId::new(), "world", None),
        ];
        let tokens = estimate_tokens(&path);
        assert!(tokens > 0);
    }

    #[test]
    fn estimate_tokens_excludes_compaction_entry() {
        // estimate_tokens_for_entry returns 0 for Compaction entries because
        // the summary is not a regular message — build_context_from_entries
        // injects it separately. Callers that need the effective context size
        // must add estimate_summary_tokens explicitly.
        let id = EntryId::new();
        let compaction = compaction_entry(id);
        assert_eq!(estimate_tokens(&[compaction]), 0);
    }

    #[test]
    fn estimate_summary_tokens_counts_summary_message() {
        // Regression: the last compaction's summary IS injected into the LLM
        // context, so estimate_summary_tokens must return a positive value.
        let id = EntryId::new();
        let compaction = compaction_entry(id);
        let tokens = estimate_summary_tokens(match &compaction.payload {
            SessionEntryPayload::Compaction(c) => c,
            _ => unreachable!(),
        });
        assert!(
            tokens > 0,
            "compaction summary must contribute tokens, got {tokens}"
        );
    }

    #[test]
    fn prepare_compaction_includes_summary_tokens_in_estimate() {
        // Regression: estimated_tokens must include the last compaction's
        // summary tokens (injected by build_context_from_entries), otherwise
        // the threshold check under-counts and compaction may not trigger.
        let kept_id = EntryId::new();
        let path = vec![
            compaction_entry(kept_id),
            user_entry(EntryId::new(), "recent question"),
        ];

        // Extract the CompactionEntry to pass as last_compaction.
        let last_compaction = match &path[0].payload {
            SessionEntryPayload::Compaction(c) => c.clone(),
            _ => unreachable!(),
        };

        // Use a tiny context window so the threshold is exceeded.
        let settings = CompactionThresholdConfig {
            reserve_tokens: 0,
            keep_recent_tokens: 0,
            context_window: 1,
        };
        let model_info = ModelInfo {
            context_window: 1,
            max_tokens: 512,
        };

        let prep = prepare_compaction(&path, Some(&last_compaction), &settings, &model_info);
        assert!(prep.is_some(), "compaction should trigger");

        let prep = prep.unwrap();
        // estimated_tokens = estimate_tokens(path) + summary_tokens.
        // estimate_tokens returns 0 for Compaction entries, so the
        // difference must equal the summary tokens.
        let base_tokens = estimate_tokens(&path);
        let summary_tokens = estimate_summary_tokens(&last_compaction);
        assert_eq!(
            prep.estimated_tokens,
            base_tokens + summary_tokens,
            "estimated_tokens must include summary tokens"
        );
    }
    #[test]
    fn compute_cut_point_returns_none_when_too_short() {
        let path = vec![user_entry(EntryId::new(), "hi")];
        let settings = CompactionThresholdConfig::with_defaults(DEFAULT_CONTEXT_WINDOW);
        assert!(compute_cut_point(&path, None, &settings, None, None).is_none());
    }

    #[test]
    fn compute_cut_point_finds_boundary_when_long_enough() {
        let path = vec![
            user_entry(EntryId::new(), "old question"),
            assistant_entry(EntryId::new(), "large reply", Some(big_token_usage(1400))),
            user_entry(EntryId::new(), "new question"),
        ];
        let settings = CompactionThresholdConfig {
            reserve_tokens: 16_384,
            keep_recent_tokens: 0,
            context_window: DEFAULT_CONTEXT_WINDOW,
        };
        let result = compute_cut_point(&path, None, &settings, None, None);
        assert!(result.is_some());
    }
    #[test]
    fn prompt_spec_is_constructible() {
        let spec = CompactionPromptSpec::new(
            "You are a summarizer.",
            "Query: {query}\nConv: {conversation}",
        )
        .unwrap();
        assert!(spec.system_prompt().contains("summarizer"));
        assert!(spec.user_template().contains("{query}"));
    }

    #[test]
    fn prompt_spec_rejects_unknown_placeholder() {
        let err = CompactionPromptSpec::new("sys", "Conv: {converstion}").unwrap_err();
        match err {
            llm_harness_types::TemplateError::UnknownPlaceholder { found, suggestion } => {
                assert_eq!(found, "converstion");
                assert_eq!(suggestion, "conversation");
            }
            other => panic!("expected UnknownPlaceholder, got {other:?}"),
        }
    }

    #[test]
    fn prompt_spec_rejects_missing_conversation_placeholder() {
        let err = CompactionPromptSpec::new("sys", "Query: {query}").unwrap_err();
        match err {
            llm_harness_types::TemplateError::MissingRequiredPlaceholder(name) => {
                assert_eq!(name, "conversation");
            }
            other => panic!("expected MissingRequiredPlaceholder, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn compact_with_custom_prompt_renders_placeholders() {
        let client = MockLlmClient::new(vec![llm_harness_loop::test_utils::MockResponse::text(
            "summary text",
        )]);

        // Build a minimal CompactionPreparation.
        let msg = AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "hello world".into(),
            }],
            timestamp: chrono::Utc::now(),
        });
        let entry = SessionEntry {
            id: EntryId::new(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            payload: SessionEntryPayload::Message(msg.clone()),
            compaction_policy: None,
        };
        let prep = CompactionPreparation {
            entries_to_compress: vec![entry],
            kept_entries: vec![],
            compress_from_entry: EntryId::new(),
            cut_point: EntryId::new(),
            previous_summary: None,
            estimated_tokens: 100,
            file_operations: vec![],
        };
        let summary = CompactionSummaryConfig {
            summary_model: "test-model".into(),
            summary_model_info: crate::agent::ModelInfo {
                context_window: 200_000,
                max_tokens: 8192,
            },
            prompt: Some(
                CompactionPromptSpec::new(
                    "Custom summarizer.",
                    "Query: {query}\nConv: {conversation}\nPrev: {previous_summary}",
                )
                .unwrap(),
            ),
        };

        let result = compact(
            &client,
            prep,
            &summary,
            Some("find bugs"),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        let _ = result;
        let captured = client.captured_requests.lock().unwrap();
        let req = &captured[0];
        // System prompt should be the custom one.
        let sys = req.messages().first().expect("has system message");
        assert!(format!("{sys:?}").contains("Custom summarizer."));
        // User content should contain the query and conversation text.
        let user_content = format!("{:?}", req.messages().get(1).expect("has user message"));
        assert!(
            user_content.contains("find bugs"),
            "query must appear in rendered prompt"
        );
        assert!(
            user_content.contains("hello world"),
            "conversation text must appear in rendered prompt"
        );
    }

    #[tokio::test]
    async fn compact_with_custom_prompt_renders_file_operations() {
        let client = MockLlmClient::new(vec![llm_harness_loop::test_utils::MockResponse::text(
            "summary text",
        )]);

        let msg = AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "hello world".into(),
            }],
            timestamp: chrono::Utc::now(),
        });
        let entry = SessionEntry {
            id: EntryId::new(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            payload: SessionEntryPayload::Message(msg.clone()),
            compaction_policy: None,
        };
        let prep = CompactionPreparation {
            entries_to_compress: vec![entry],
            kept_entries: vec![],
            compress_from_entry: EntryId::new(),
            cut_point: EntryId::new(),
            previous_summary: None,
            estimated_tokens: 100,
            file_operations: vec![
                FileOperation {
                    path: PathBuf::from("/src/main.rs"),
                    kind: FileOpKind::Read,
                    at_entry: EntryId::new(),
                },
                FileOperation {
                    path: PathBuf::from("/src/lib.rs"),
                    kind: FileOpKind::Modify,
                    at_entry: EntryId::new(),
                },
            ],
        };
        let summary = CompactionSummaryConfig {
            summary_model: "test-model".into(),
            summary_model_info: crate::agent::ModelInfo {
                context_window: 200_000,
                max_tokens: 8192,
            },
            prompt: Some(
                CompactionPromptSpec::new(
                    "Custom summarizer.",
                    "Conv: {conversation}\nFiles: {file_operations}",
                )
                .unwrap(),
            ),
        };

        let _ = compact(&client, prep, &summary, None, &CancellationToken::new())
            .await
            .unwrap();

        let captured = client.captured_requests.lock().unwrap();
        let req = &captured[0];
        let user_content = format!("{:?}", req.messages().get(1).expect("has user message"));
        assert!(
            user_content.contains("/src/main.rs"),
            "file_operations must render file paths"
        );
        assert!(
            user_content.contains("/src/lib.rs"),
            "file_operations must render all file paths"
        );
        assert!(
            user_content.contains("Read"),
            "file_operations must render operation kind"
        );
        assert!(
            user_content.contains("Modify"),
            "file_operations must render Modify kind"
        );
    }
}
