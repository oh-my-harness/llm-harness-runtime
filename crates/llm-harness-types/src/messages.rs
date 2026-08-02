use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ContentBlock, DataBlock, PersistedToolProjection, StopReason};

/// Stable identity for the canonical token-accounting semantics.
pub const BILLING_USAGE_PROFILE_ID: &str = "llm-harness-billing-usage";
/// Version of the canonical token-accounting semantics.
pub const BILLING_USAGE_PROFILE_VERSION: &str = "1";
/// Canonical, digest-bound definition of each usage category.
pub const BILLING_USAGE_PROFILE_CONTRACT: &str = "input_tokens=gross_input;cache_read_tokens=input_subset;cache_creation_input_tokens=input_subset;cache_read_and_creation=disjoint;output_tokens=gross_output;reasoning_tokens=output_subset;uncached_input=input_tokens-cache_read_tokens-cache_creation_input_tokens";
/// SHA-256 digest of [`BILLING_USAGE_PROFILE_CONTRACT`].
pub const BILLING_USAGE_PROFILE_DIGEST: &str =
    "sha256:6afb4df926d32f3917ab28fb7ea30af87b20d7b81391d800c076f637c7eb73fb";
/// Stable identity for provider usage-report provenance semantics.
pub const USAGE_REPORT_PROFILE_ID: &str = "llm-harness-usage-report";
/// Version of the provider usage-report provenance semantics.
pub const USAGE_REPORT_PROFILE_VERSION: &str = "1";
/// Canonical, digest-bound definition of usage-report provenance.
pub const USAGE_REPORT_PROFILE_CONTRACT: &str = "provenance=provider_payload;states=reported_valid,missing,reported_invalid;zero_tokens_do_not_imply_missing;reported_valid_requires_billing_invariants";
/// SHA-256 digest of [`USAGE_REPORT_PROFILE_CONTRACT`].
pub const USAGE_REPORT_PROFILE_DIGEST: &str =
    "sha256:f6a776c595f99114eb692faf4f72eb4eb0012a33fa3722244e65fad0d6f48073";

/// Compact runtime marker for the accounting semantics used by an aggregate.
///
/// The full ID, version, and digest are exposed by the accessors and are bound
/// separately into persisted trusted-pricing admissions.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BillingUsageProfile {
    /// Canonical gross-input, mutually exclusive cache-subset profile.
    V1,
}

impl BillingUsageProfile {
    /// Stable profile ID.
    pub fn id(&self) -> &str {
        BILLING_USAGE_PROFILE_ID
    }

    /// Stable profile version.
    pub fn version(&self) -> &str {
        BILLING_USAGE_PROFILE_VERSION
    }

    /// Digest of the canonical profile contract.
    pub fn digest(&self) -> &str {
        BILLING_USAGE_PROFILE_DIGEST
    }

    /// Returns whether this identity is the runtime's canonical profile.
    pub fn is_canonical(&self) -> bool {
        matches!(self, Self::V1)
    }
}

/// Returns the verifier-owned canonical token-accounting profile.
pub fn canonical_billing_usage_profile() -> BillingUsageProfile {
    BillingUsageProfile::V1
}

/// Compact runtime marker for provider usage-report provenance semantics.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum UsageReportProfile {
    /// Provider-payload provenance with explicit valid/missing/invalid states.
    V1,
}

impl UsageReportProfile {
    /// Stable profile ID.
    pub fn id(&self) -> &str {
        USAGE_REPORT_PROFILE_ID
    }

    /// Stable profile version.
    pub fn version(&self) -> &str {
        USAGE_REPORT_PROFILE_VERSION
    }

    /// Digest of the canonical provenance contract.
    pub fn digest(&self) -> &str {
        USAGE_REPORT_PROFILE_DIGEST
    }

    /// Returns whether this identity is runtime's canonical profile.
    pub fn is_canonical(&self) -> bool {
        matches!(self, Self::V1)
    }
}

/// Returns the verifier-owned canonical usage-report profile.
pub fn canonical_usage_report_profile() -> UsageReportProfile {
    UsageReportProfile::V1
}

/// Provenance and validity of one provider usage report.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UsageReportProvenance {
    /// The response completed without authoritative provider usage.
    #[default]
    Missing,
    /// The provider supplied usage and canonical invariants hold.
    ReportedValid,
    /// The provider supplied usage but an invariant failed.
    ReportedInvalid,
}

/// A provider usage payload violates the canonical accounting profile.
#[derive(Debug, Clone, Eq, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct BillingUsageProfileError {
    message: String,
}

impl BillingUsageProfileError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Token usage for one LLM request/response pair.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenUsage {
    /// Gross prompt tokens, including cache reads and cache writes.
    pub input_tokens: u32,
    /// Gross output/completion tokens, including reasoning tokens.
    pub output_tokens: u32,
    /// Cache-read tokens; a subset of `input_tokens`.
    pub cache_read_tokens: u32,
    /// Cache-write tokens; a disjoint subset of `input_tokens`.
    pub cache_creation_tokens: u32,
    /// Reasoning/thinking tokens; a subset of `output_tokens`.
    pub reasoning_tokens: u32,
    /// Provider-report provenance. Token values never imply this state.
    #[serde(default)]
    pub provenance: UsageReportProvenance,
}

/// A provider usage-report aggregate violates the provenance contract.
#[derive(Debug, Clone, Eq, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct UsageReportProfileError {
    message: String,
}

impl UsageReportProfileError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl TokenUsage {
    /// Gross input plus gross output, without double-counting subsets.
    pub fn total_tokens(&self) -> u32 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// Prompt tokens billed at the normal, non-cache input rate.
    pub fn billable_uncached_input_tokens(&self) -> Result<u32, BillingUsageProfileError> {
        let cached = self
            .cache_read_tokens
            .checked_add(self.cache_creation_tokens)
            .ok_or_else(|| {
                BillingUsageProfileError::new("cache input token categories overflow")
            })?;
        self.input_tokens.checked_sub(cached).ok_or_else(|| {
            BillingUsageProfileError::new(format!(
                "gross input tokens ({}) are smaller than cache read plus cache creation tokens ({cached})",
                self.input_tokens
            ))
        })
    }

    /// Validates the canonical gross-plus-exclusive-subsets profile.
    pub fn validate_billing_profile(&self) -> Result<(), BillingUsageProfileError> {
        self.billable_uncached_input_tokens()?;
        if self.reasoning_tokens > self.output_tokens {
            return Err(BillingUsageProfileError::new(format!(
                "reasoning tokens ({}) exceed gross output tokens ({})",
                self.reasoning_tokens, self.output_tokens
            )));
        }
        Ok(())
    }

    /// Requires an authoritative, numerically valid provider report.
    pub fn validate_report(&self) -> Result<(), BillingUsageProfileError> {
        match self.provenance {
            UsageReportProvenance::ReportedValid => self.validate_billing_profile(),
            UsageReportProvenance::Missing => Err(BillingUsageProfileError::new(
                "provider usage report is missing",
            )),
            UsageReportProvenance::ReportedInvalid => Err(BillingUsageProfileError::new(
                "provider usage report is invalid",
            )),
        }
    }
}

/// Assistant message display and persistence semantics.
///
/// UIs should render `FinalAnswer` as the durable assistant response for a
/// user turn. `Progress` is model-visible context and may be shown in progress
/// views, but should not be restored as the final assistant bubble.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AssistantMessageKind {
    /// Intermediate assistant text, usually followed by tool calls or another turn.
    Progress,
    /// Durable user-visible answer for the turn.
    #[default]
    FinalAnswer,
}

/// 用户消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage {
    /// 消息内容块列表。
    pub content: Vec<ContentBlock>,
    /// 消息创建时间。
    pub timestamp: DateTime<Utc>,
}

/// LLM 助手消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// UI/display semantics for this assistant message.
    #[serde(default)]
    pub kind: AssistantMessageKind,
    /// Stable ID for this assistant message when produced by the runtime loop.
    pub message_id: String,
    /// Stable ID for the user turn that produced this assistant message.
    pub turn_id: String,
    /// 消息内容块列表（含 Text、Thinking、ToolUse 等）。
    pub content: Vec<ContentBlock>,
    /// LLM 停止生成的原因。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    /// 消息创建时间。
    pub timestamp: DateTime<Utc>,
    /// 生成此消息的 LLM provider 名（如 `"anthropic"`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// 使用的 API 类型（如 `"messages"`、`"chat"`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// 使用的模型 ID。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 本次调用的 token 用量；compaction 估算依赖此字段。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    /// LLM 返回错误时保存的错误文本快照。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

impl AssistantMessage {
    /// Concatenate user-visible text blocks, excluding thinking and tool-use blocks.
    pub fn text_content(&self) -> String {
        let mut text = String::new();
        for block in &self.content {
            if let ContentBlock::Text { text: block_text } = block {
                text.push_str(block_text);
            }
        }
        text
    }
}

/// 工具执行结果消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultMessage {
    /// 对应的 LLM tool call ID。
    pub tool_use_id: String,
    /// 发送给 LLM 的结果内容块。
    pub content: Vec<DataBlock>,
    /// 工具执行是否失败。
    pub is_error: bool,
    /// How this message was projected from the current-run tool result.
    pub projection: PersistedToolProjection,
    /// 消息创建时间。
    pub timestamp: DateTime<Utc>,
}

/// Compaction 生成的摘要消息；由框架在 compaction 时插入。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionSummaryMessage {
    /// 摘要文本。
    pub summary: String,
    /// 消息创建时间。
    pub timestamp: DateTime<Utc>,
}

/// 分支导航时生成的摘要消息；由框架在 navigate_tree 时插入。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchSummaryMessage {
    /// 摘要文本。
    pub summary: String,
    /// 消息创建时间。
    pub timestamp: DateTime<Utc>,
}

/// 应用层自定义消息；必须由 `ConvertToLlmHook` 提供转换器才能进入 LLM 上下文。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomMessage {
    /// 应用层自定义的消息类别标签（如 `"artifact"`、`"notification"`）。
    pub r#type: String,
    /// 任意 JSON 负载。
    pub data: serde_json::Value,
    /// 消息创建时间。
    pub timestamp: DateTime<Utc>,
}

/// Agent 内部消息联合体；会进入 session log 并经 compaction 处理。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum AgentMessage {
    /// 用户消息。
    User(UserMessage),
    /// LLM 助手消息。
    Assistant(AssistantMessage),
    /// 工具执行结果消息。
    ToolResult(ToolResultMessage),
    /// 框架生成的分支摘要消息。
    BranchSummary(BranchSummaryMessage),
    /// 框架生成的 compaction 摘要消息。
    CompactionSummary(CompactionSummaryMessage),
    /// 应用层自定义消息。
    Custom(CustomMessage),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContentBlock;
    use sha2::{Digest, Sha256};

    #[test]
    fn profile_digests_match_their_contract_text() {
        let billing = format!(
            "sha256:{:x}",
            Sha256::digest(BILLING_USAGE_PROFILE_CONTRACT.as_bytes())
        );
        let provenance = format!(
            "sha256:{:x}",
            Sha256::digest(USAGE_REPORT_PROFILE_CONTRACT.as_bytes())
        );

        assert_eq!(billing, BILLING_USAGE_PROFILE_DIGEST);
        assert_eq!(provenance, USAGE_REPORT_PROFILE_DIGEST);
    }

    fn text_block(s: &str) -> ContentBlock {
        ContentBlock::Text { text: s.into() }
    }

    #[test]
    fn agent_message_serde_user() {
        let msg = AgentMessage::User(UserMessage {
            content: vec![text_block("hello")],
            timestamp: chrono::Utc::now(),
        });
        let json = serde_json::to_string(&msg).unwrap();
        let msg2: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(msg2, AgentMessage::User(_)));
    }

    #[test]
    fn agent_message_serde_assistant() {
        let msg = AgentMessage::Assistant(AssistantMessage {
            kind: AssistantMessageKind::FinalAnswer,
            message_id: "test_msg".into(),
            turn_id: "test_turn".into(),
            content: vec![text_block("response")],
            stop_reason: Some(crate::StopReason::EndTurn),
            timestamp: chrono::Utc::now(),
            provider: Some("anthropic".into()),
            api: Some("messages".into()),
            model: Some("claude-3".into()),
            usage: Some(TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                reasoning_tokens: 0,
                provenance: UsageReportProvenance::ReportedValid,
            }),
            error_message: None,
        });
        let json = serde_json::to_string(&msg).unwrap();
        let msg2: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(msg2, AgentMessage::Assistant(_)));
    }

    #[test]
    fn tool_result_message_serde() {
        let msg = AgentMessage::ToolResult(ToolResultMessage {
            tool_use_id: "call_1".into(),
            content: vec![DataBlock::text("result")],
            is_error: false,
            projection: PersistedToolProjection {
                kind: crate::ToolProjectionKind::Full,
                metadata: crate::ProjectionMetadata::default(),
            },
            timestamp: chrono::Utc::now(),
        });
        let json = serde_json::to_string(&msg).unwrap();
        let msg2: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(msg2, AgentMessage::ToolResult(_)));
    }

    #[test]
    fn custom_message_serde() {
        let msg = AgentMessage::Custom(CustomMessage {
            r#type: "artifact".into(),
            data: serde_json::json!({"url": "https://example.com"}),
            timestamp: chrono::Utc::now(),
        });
        let json = serde_json::to_string(&msg).unwrap();
        let msg2: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(msg2, AgentMessage::Custom(_)));
    }

    #[test]
    fn token_usage_total() {
        let u = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 20,
            cache_creation_tokens: 10,
            reasoning_tokens: 20,
            provenance: UsageReportProvenance::ReportedValid,
        };
        assert_eq!(u.total_tokens(), 150);
        assert_eq!(u.billable_uncached_input_tokens().unwrap(), 70);
        u.validate_billing_profile().unwrap();
    }

    #[test]
    fn token_usage_rejects_invalid_billing_subsets() {
        let cache_error = TokenUsage {
            input_tokens: 100,
            cache_read_tokens: 80,
            cache_creation_tokens: 30,
            ..Default::default()
        }
        .validate_billing_profile()
        .unwrap_err();
        assert!(cache_error.to_string().contains("smaller than cache read"));

        let reasoning_error = TokenUsage {
            output_tokens: 20,
            reasoning_tokens: 21,
            ..Default::default()
        }
        .validate_billing_profile()
        .unwrap_err();
        assert!(reasoning_error.to_string().contains("exceed gross output"));
    }

    #[test]
    fn token_usage_report_provenance_is_independent_of_zero_values() {
        let missing = TokenUsage::default();
        assert!(missing.validate_report().is_err());

        let reported_zero = TokenUsage {
            provenance: UsageReportProvenance::ReportedValid,
            ..Default::default()
        };
        reported_zero
            .validate_report()
            .expect("an explicit provider zero payload is valid evidence");

        let reported_invalid = TokenUsage {
            provenance: UsageReportProvenance::ReportedInvalid,
            ..Default::default()
        };
        assert!(reported_invalid.validate_report().is_err());
    }

    #[test]
    fn assistant_message_none_fields_omitted_in_json() {
        let msg = AssistantMessage {
            kind: AssistantMessageKind::FinalAnswer,
            message_id: "test_msg".into(),
            turn_id: "test_turn".into(),
            content: vec![text_block("hi")],
            stop_reason: None,
            timestamp: chrono::Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        // 逐字段断言 None 值被省略，而非用 "null" 子串匹配（防误报）
        assert!(
            !json.contains(r#""stop_reason""#),
            "stop_reason should be omitted: {json}"
        );
        assert!(
            !json.contains(r#""provider""#),
            "provider should be omitted: {json}"
        );
        assert!(!json.contains(r#""api""#), "api should be omitted: {json}");
        assert!(
            !json.contains(r#""model""#),
            "model should be omitted: {json}"
        );
        assert!(
            !json.contains(r#""usage""#),
            "usage should be omitted: {json}"
        );
        assert!(
            !json.contains(r#""error_message""#),
            "error_message should be omitted: {json}"
        );
        // round-trip
        let msg2: AssistantMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg2.kind, AssistantMessageKind::FinalAnswer);
        assert!(msg2.stop_reason.is_none());
        assert!(msg2.provider.is_none());
    }

    #[test]
    fn assistant_message_kind_and_ids_roundtrip() {
        let msg = AssistantMessage {
            kind: AssistantMessageKind::Progress,
            message_id: "msg_1".into(),
            turn_id: "turn_1".into(),
            content: vec![text_block("checking")],
            stop_reason: Some(crate::StopReason::ToolUse),
            timestamp: chrono::Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        };

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""kind":"progress""#), "{json}");
        assert!(json.contains(r#""message_id":"msg_1""#), "{json}");
        assert!(json.contains(r#""turn_id":"turn_1""#), "{json}");

        let decoded: AssistantMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.kind, AssistantMessageKind::Progress);
        assert_eq!(decoded.message_id, "msg_1");
        assert_eq!(decoded.turn_id, "turn_1");
    }

    #[test]
    fn assistant_message_missing_kind_defaults_to_final_answer() {
        let json = serde_json::json!({
            "content": [{"type": "text", "text": "legacy"}],
            "timestamp": chrono::Utc::now(),
            "message_id": "msg_1",
            "turn_id": "turn_1",
        });

        let decoded: AssistantMessage = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.kind, AssistantMessageKind::FinalAnswer);
    }

    #[test]
    fn assistant_message_text_content_concatenates_visible_text_only() {
        let msg = AssistantMessage {
            kind: AssistantMessageKind::FinalAnswer,
            message_id: "test_msg".into(),
            turn_id: "test_turn".into(),
            content: vec![
                text_block("hello "),
                ContentBlock::Thinking {
                    thinking: "hidden".into(),
                    signature: None,
                },
                text_block("world"),
            ],
            stop_reason: Some(crate::StopReason::EndTurn),
            timestamp: chrono::Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        };

        assert_eq!(msg.text_content(), "hello world");
    }
}
