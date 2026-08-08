//! Context-aware compaction prompt template.
//!
//! Provides a default [`CompactionPromptSpec`] that weaves `{query}` into the
//! instruction body (not a prefix) and encodes the tutorial's 5-level retention
//! priority. Users inject it via `summary.prompt = Some(context_aware_prompt_spec())`.

use llm_harness_agent::CompactionPromptSpec;

/// System prompt for context-aware compaction.
const SYSTEM_PROMPT: &str = "\
You are a context summarization assistant. Your task is to read a conversation \
between a user and an AI coding assistant, then produce a structured summary \
that another LLM instance will use to continue the work seamlessly.\n\
\n\
Be concise but complete. When deciding what to preserve, prioritize information \
relevant to the task intent and apply this retention priority (highest first):\n\
1. Architecture decisions and key constraints — never summarize, preserve verbatim\n\
2. Modified file list and key change records — preserve complete\n\
3. Verification status (pass/fail) — must preserve\n\
4. Unresolved TODOs and rollback notes — must preserve\n\
5. Tool outputs — can be deleted, keep only pass/fail conclusions\n\
\n\
Identifiers (file paths, function names, error messages, UUIDs, hashes, URLs) \
must be preserved verbatim — do not paraphrase or truncate them.\n";

/// User message template for context-aware compaction.
///
/// `{query}` and `{previous_summary}` are woven into the instruction body.
/// When empty, they degrade gracefully (half-empty line / blank line).
/// `{file_operations}` is intentionally omitted — `compact()` appends
/// `## Files Touched` at the end, so including it here would duplicate
/// the heading.
const USER_TEMPLATE: &str = "\
You are compressing a conversation so another LLM can continue the task seamlessly.\n\
\n\
Task intent: {query}\n\
\n\
{previous_summary}\n\
\n\
Below is the conversation to compress:\n\
<conversation>\n\
{conversation}\n\
</conversation>\n\
\n\
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

/// Return a context-aware `CompactionPromptSpec` for compaction summaries.
///
/// Compared to the mechanism layer's hardcoded constants (used when
/// `prompt` is `None`), this template:
/// - weaves `{query}` into the instruction body (not a prefix), guiding
///   the LLM to prioritize by task intent
/// - encodes the tutorial's 5-level retention priority
/// - `{previous_summary}` appears conditionally (empty value degrades to
///   a blank line)
///
/// Users inject it via `summary.prompt = Some(context_aware_prompt_spec())`.
///
/// # Panics
///
/// Panics if the template constants are malformed (e.g. unknown placeholder).
/// This is a compile-time-constant bug — the constants above are validated
/// at construction.
pub fn context_aware_prompt_spec() -> CompactionPromptSpec {
    CompactionPromptSpec::new(SYSTEM_PROMPT, USER_TEMPLATE)
        .expect("SYSTEM_PROMPT and USER_TEMPLATE must be valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_valid_spec() {
        // CompactionPromptSpec::new validates placeholders — if the template
        // has an unknown placeholder or is missing {conversation}, new()
        // returns Err and expect() panics.
        let spec = context_aware_prompt_spec();
        assert!(spec.user_template().contains("{conversation}"));
    }

    #[test]
    fn contains_expected_placeholders() {
        let spec = context_aware_prompt_spec();
        let template = spec.user_template();
        assert!(template.contains("{conversation}"));
        assert!(template.contains("{query}"));
        assert!(template.contains("{previous_summary}"));
    }

    #[test]
    fn does_not_contain_file_operations() {
        let spec = context_aware_prompt_spec();
        // {file_operations} is intentionally omitted — compact() appends
        // ## Files Touched at the end, including it here would duplicate.
        assert!(!spec.user_template().contains("{file_operations}"));
    }

    #[test]
    fn user_template_has_required_structure() {
        let spec = context_aware_prompt_spec();
        let template = spec.user_template();
        assert!(template.contains("<conversation>"));
        assert!(template.contains("{conversation}"));
        assert!(template.contains("</conversation>"));
        assert!(template.contains("Task intent:"));
        assert!(template.contains("## Goal"));
        assert!(template.contains("## Progress"));
        assert!(template.contains("## Key Decisions"));
        assert!(template.contains("## Next Steps"));
        assert!(template.contains("## Critical Context"));
    }

    #[test]
    fn system_prompt_has_retention_priority_and_identifiers() {
        let spec = context_aware_prompt_spec();
        let sys = spec.system_prompt();
        assert!(sys.contains("retention priority"));
        assert!(sys.contains("Identifiers"));
    }

    #[test]
    fn query_placeholder_prefixed_with_task_intent() {
        // Verify the template structure that enables graceful empty-value
        // degradation: {query} is prefixed with "Task intent: " so an empty
        // query produces "Task intent: " (a half-empty line, harmless).
        let spec = context_aware_prompt_spec();
        assert!(spec.user_template().contains("Task intent: {query}"));
    }

    #[test]
    fn previous_summary_on_own_line() {
        // {previous_summary} is on its own line so an empty value degrades
        // to a blank line (harmless).
        let spec = context_aware_prompt_spec();
        assert!(spec.user_template().contains("\n{previous_summary}\n"));
    }

    #[test]
    fn system_prompt_does_not_contain_no_compact() {
        // <no-compact> instruction is auto-appended by compact() via
        // NO_COMPACT_INSTRUCTION. The template must not duplicate it.
        let spec = context_aware_prompt_spec();
        assert!(!spec.system_prompt().contains("no-compact"));
    }
}
