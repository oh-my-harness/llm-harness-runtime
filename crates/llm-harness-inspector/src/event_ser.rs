//! Event serialization projection for the Inspector Web API.
//!
//! [`AgentHarnessEvent`] and [`AgentEvent`] only derive `Debug` — they cannot be
//! serialized directly because some nested types (e.g. `ToolResult.extensions`,
//! which is a `HashMap<TypeId, Arc<dyn Any>>`) are fundamentally process-local.
//!
//! This module defines serializable projection types that mirror every variant
//! while dropping non-serializable fields. The projection is 1:1 at the variant
//! level: no events are renamed, merged, or filtered out.

use chrono::{DateTime, Utc};
use serde::{Serialize, Serializer};

use llm_harness_agent::{AgentHarnessEvent, CompactionStats, HarnessState, HarnessToolCallResult};
use llm_harness_types::{
    AgentEvent, CostAggregate, DataBlock, DiagnosticLevel, EntryId, HarnessPhase, SkillDiagnostic,
    ThinkingLevel, ToolFailure, ToolProgress, ToolResult, ToolSessionProjection,
};

// ── Projection wrappers for non-serializable types ───────────────────────────

/// Serializable projection of [`ToolResult`].
///
/// Drops `extensions` (the `TypeId`-keyed `Arc<dyn Any>` map — process-local,
/// no cross-process semantics).
#[derive(Debug, Clone, Serialize)]
pub struct SerializableToolResult {
    pub model_content: Vec<DataBlock>,
    pub details: serde_json::Value,
    pub terminate: bool,
    pub session_projection: ToolSessionProjection,
}

impl From<&ToolResult> for SerializableToolResult {
    fn from(r: &ToolResult) -> Self {
        Self {
            model_content: r.model_content.clone(),
            details: r.details.clone(),
            terminate: r.terminate,
            session_projection: r.session_projection.clone(),
        }
    }
}

/// Serializable projection of [`ToolFailure`].
#[derive(Debug, Clone, Serialize)]
pub struct SerializableToolFailure {
    pub code: String,
    pub model_message: String,
    pub retryable: bool,
    pub details: serde_json::Value,
    pub session_projection: ToolSessionProjection,
}

impl From<&ToolFailure> for SerializableToolFailure {
    fn from(f: &ToolFailure) -> Self {
        Self {
            code: f.code.clone(),
            model_message: f.model_message.clone(),
            retryable: f.retryable,
            details: f.details.clone(),
            session_projection: f.session_projection.clone(),
        }
    }
}

/// Serializable projection of [`ToolProgress`].
#[derive(Debug, Clone, Serialize)]
pub struct SerializableToolProgress {
    pub content: Vec<DataBlock>,
    pub details: serde_json::Value,
}

impl From<&ToolProgress> for SerializableToolProgress {
    fn from(p: &ToolProgress) -> Self {
        Self {
            content: p.content.clone(),
            details: p.details.clone(),
        }
    }
}

/// Serializable projection of `Result<ToolResult, ToolFailure>`.
///
/// Uses `#[serde(untagged)]` so the JSON shape is either the Ok or Err variant
/// without an explicit discriminator — matching how `Result` is naturally
/// consumed.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum SerializableToolOutcome {
    Ok(SerializableToolResult),
    Err(SerializableToolFailure),
}

impl From<&Result<ToolResult, ToolFailure>> for SerializableToolOutcome {
    fn from(r: &Result<ToolResult, ToolFailure>) -> Self {
        match r {
            Ok(tr) => Self::Ok(SerializableToolResult::from(tr)),
            Err(tf) => Self::Err(SerializableToolFailure::from(tf)),
        }
    }
}

/// Serializable projection of [`CompactionStats`].
#[derive(Debug, Clone, Serialize)]
pub struct SerializableCompactionStats {
    pub tokens_before: usize,
    pub tokens_after: usize,
    pub compressed_entries: usize,
}

impl From<&CompactionStats> for SerializableCompactionStats {
    fn from(s: &CompactionStats) -> Self {
        Self {
            tokens_before: s.tokens_before,
            tokens_after: s.tokens_after,
            compressed_entries: s.compressed_entries,
        }
    }
}

/// Serializable projection of [`HarnessToolCallResult`].
#[derive(Debug, Clone, Serialize)]
pub struct SerializableHarnessToolCallResult {
    pub details: serde_json::Value,
    pub is_error: bool,
}

impl From<&HarnessToolCallResult> for SerializableHarnessToolCallResult {
    fn from(r: &HarnessToolCallResult) -> Self {
        Self {
            details: r.details.clone(),
            is_error: r.is_error,
        }
    }
}

/// Serializable projection of [`SkillDiagnostic`].
///
/// `PathBuf` and `DiagnosticLevel` are projected to `String`.
#[derive(Debug, Clone, Serialize)]
pub struct SerializableSkillDiagnostic {
    pub source: String,
    pub level: String,
    pub message: String,
}

impl From<&SkillDiagnostic> for SerializableSkillDiagnostic {
    fn from(d: &SkillDiagnostic) -> Self {
        Self {
            source: d.source.display().to_string(),
            level: diagnostic_level_str(d.level).to_string(),
            message: d.message.clone(),
        }
    }
}

fn diagnostic_level_str(level: DiagnosticLevel) -> &'static str {
    match level {
        DiagnosticLevel::Warn => "Warn",
        DiagnosticLevel::Error => "Error",
    }
}

/// Serializable projection of [`HarnessPhase`].
fn phase_str(phase: HarnessPhase) -> &'static str {
    match phase {
        HarnessPhase::Idle => "Idle",
        HarnessPhase::Turning => "Turning",
        HarnessPhase::Compacting => "Compacting",
        HarnessPhase::Branching => "Branching",
        HarnessPhase::Resetting => "Resetting",
        _ => "Unknown",
    }
}

// ── SerializableAgentEvent ───────────────────────────────────────────────────

/// Serializable projection of [`AgentEvent`].
///
/// Each variant maps 1:1. `ToolResult.extensions` is dropped.
/// `AgentError` is projected to its `Display` string.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind")]
pub enum SerializableAgentEvent {
    AgentStart,
    AgentEnd {
        new_messages: Vec<llm_harness_types::AgentMessage>,
    },
    TurnStart {
        index: u32,
    },
    TurnEnd {
        index: u32,
        message: llm_harness_types::AssistantMessage,
        tool_results: Vec<(String, SerializableToolOutcome)>,
    },
    MessageStart {
        message_id: String,
    },
    MessageUpdate {
        message_id: String,
        partial: llm_harness_types::AssistantMessage,
    },
    MessageEnd {
        message_id: String,
        message: llm_harness_types::AssistantMessage,
        discarded: bool,
    },
    TextDelta {
        message_id: String,
        text: String,
    },
    ThinkingDelta {
        message_id: String,
        thinking: String,
        signature: Option<String>,
    },
    ToolCallStart {
        message_id: String,
        tool_use_id: String,
        name: String,
    },
    ToolCallArgsDelta {
        tool_use_id: String,
        partial_input: String,
    },
    ToolCallEnd {
        tool_use_id: String,
        args: serde_json::Value,
    },
    ToolExecutionStart {
        tool_use_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    ToolExecutionUpdate {
        tool_use_id: String,
        partial: SerializableToolProgress,
    },
    ToolExecutionEnd {
        tool_use_id: String,
        result: SerializableToolOutcome,
    },
    Error {
        error: String,
    },
    RetryAttempt {
        attempt: u32,
        max_retries: u32,
        delay_ms: u64,
        error: String,
    },
    /// Catch-all for unknown `#[non_exhaustive]` variants.
    /// Preserves the Debug string so consumers can inspect without mistaking
    /// it for an error.
    Unknown {
        debug: String,
    },
}

impl From<&AgentEvent> for SerializableAgentEvent {
    fn from(e: &AgentEvent) -> Self {
        match e {
            AgentEvent::AgentStart => Self::AgentStart,
            AgentEvent::AgentEnd { new_messages } => Self::AgentEnd {
                new_messages: new_messages.clone(),
            },
            AgentEvent::TurnStart { index } => Self::TurnStart { index: *index },
            AgentEvent::TurnEnd {
                index,
                message,
                tool_results,
            } => Self::TurnEnd {
                index: *index,
                message: message.clone(),
                tool_results: tool_results
                    .iter()
                    .map(|(id, r)| (id.clone(), SerializableToolOutcome::from(r)))
                    .collect(),
            },
            AgentEvent::MessageStart { message_id } => Self::MessageStart {
                message_id: message_id.clone(),
            },
            AgentEvent::MessageUpdate {
                message_id,
                partial,
            } => Self::MessageUpdate {
                message_id: message_id.clone(),
                partial: partial.clone(),
            },
            AgentEvent::MessageEnd {
                message_id,
                message,
                discarded,
            } => Self::MessageEnd {
                message_id: message_id.clone(),
                message: message.clone(),
                discarded: *discarded,
            },
            AgentEvent::TextDelta { message_id, text } => Self::TextDelta {
                message_id: message_id.clone(),
                text: text.clone(),
            },
            AgentEvent::ThinkingDelta {
                message_id,
                thinking,
                signature,
            } => Self::ThinkingDelta {
                message_id: message_id.clone(),
                thinking: thinking.clone(),
                signature: signature.clone(),
            },
            AgentEvent::ToolCallStart {
                message_id,
                tool_use_id,
                name,
            } => Self::ToolCallStart {
                message_id: message_id.clone(),
                tool_use_id: tool_use_id.clone(),
                name: name.clone(),
            },
            AgentEvent::ToolCallArgsDelta {
                tool_use_id,
                partial_input,
            } => Self::ToolCallArgsDelta {
                tool_use_id: tool_use_id.clone(),
                partial_input: partial_input.clone(),
            },
            AgentEvent::ToolCallEnd { tool_use_id, args } => Self::ToolCallEnd {
                tool_use_id: tool_use_id.clone(),
                args: args.clone(),
            },
            AgentEvent::ToolExecutionStart {
                tool_use_id,
                tool_name,
                args,
            } => Self::ToolExecutionStart {
                tool_use_id: tool_use_id.clone(),
                tool_name: tool_name.clone(),
                args: args.clone(),
            },
            AgentEvent::ToolExecutionUpdate {
                tool_use_id,
                partial,
            } => Self::ToolExecutionUpdate {
                tool_use_id: tool_use_id.clone(),
                partial: SerializableToolProgress::from(partial),
            },
            AgentEvent::ToolExecutionEnd {
                tool_use_id,
                result,
            } => Self::ToolExecutionEnd {
                tool_use_id: tool_use_id.clone(),
                result: SerializableToolOutcome::from(result),
            },
            AgentEvent::Error(err) => Self::Error {
                error: err.to_string(),
            },
            AgentEvent::RetryAttempt {
                attempt,
                max_retries,
                delay_ms,
                error,
            } => Self::RetryAttempt {
                attempt: *attempt,
                max_retries: *max_retries,
                delay_ms: *delay_ms,
                error: error.clone(),
            },
            // AgentEvent is #[non_exhaustive]; unknown variants are projected
            // as Unknown (not Error) to preserve event semantics.
            other => Self::Unknown {
                debug: format!("{:?}", other),
            },
        }
    }
}

// ── SerializableAgentHarnessEvent ────────────────────────────────────────────

/// Serializable projection of [`AgentHarnessEvent`].
///
/// Each variant maps 1:1. `HarnessPhase` is projected to its Debug string.
/// `ThinkingLevel` is projected to its Debug string for consistency.
/// `HashSet<String>` becomes `Vec<String>`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum SerializableAgentHarnessEvent {
    Agent {
        inner: SerializableAgentEvent,
    },
    PhaseChange {
        from: String,
        to: String,
    },
    ModelUpdate {
        from: String,
        to: String,
    },
    ThinkingLevelUpdate {
        from: String,
        to: String,
    },
    ToolsUpdate {
        added: Vec<String>,
        removed: Vec<String>,
    },
    ActiveToolsUpdate {
        active: Option<Vec<String>>,
    },
    ResourcesUpdate {
        skills: usize,
        templates: usize,
        diagnostics: Vec<SerializableSkillDiagnostic>,
    },
    SessionInfoUpdate {
        name: String,
    },
    CompactionStart {
        estimated_tokens: usize,
    },
    CompactionEnd {
        stats: Option<SerializableCompactionStats>,
        error: Option<String>,
    },
    QueueUpdate {
        steer_len: usize,
        follow_up_len: usize,
    },
    SavePoint {
        entries_flushed: usize,
    },
    BranchForked {
        from: EntryId,
        new_leaf: EntryId,
        label: Option<String>,
    },
    BranchSwitched {
        from: EntryId,
        to: EntryId,
    },
    BranchDeleted {
        leaf: EntryId,
    },
    BranchSummarized {
        leaf: EntryId,
        summary: String,
    },
    ToolCallStart {
        tool_use_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    ToolCallEnd {
        tool_use_id: String,
        tool_name: String,
        result: SerializableHarnessToolCallResult,
    },
    Settled,
    Aborted,
}

impl From<&AgentHarnessEvent> for SerializableAgentHarnessEvent {
    fn from(e: &AgentHarnessEvent) -> Self {
        match e {
            AgentHarnessEvent::Agent(inner) => Self::Agent {
                inner: SerializableAgentEvent::from(inner),
            },
            AgentHarnessEvent::PhaseChange { from, to } => Self::PhaseChange {
                from: phase_str(*from).to_string(),
                to: phase_str(*to).to_string(),
            },
            AgentHarnessEvent::ModelUpdate { from, to } => Self::ModelUpdate {
                from: from.clone(),
                to: to.clone(),
            },
            AgentHarnessEvent::ThinkingLevelUpdate { from, to } => Self::ThinkingLevelUpdate {
                from: thinking_level_str(*from).to_string(),
                to: thinking_level_str(*to).to_string(),
            },
            AgentHarnessEvent::ToolsUpdate { added, removed } => Self::ToolsUpdate {
                added: added.clone(),
                removed: removed.clone(),
            },
            AgentHarnessEvent::ActiveToolsUpdate { active } => Self::ActiveToolsUpdate {
                active: active.as_ref().map(|s| {
                    let mut v: Vec<String> = s.iter().cloned().collect();
                    v.sort();
                    v
                }),
            },
            AgentHarnessEvent::ResourcesUpdate {
                skills,
                templates,
                diagnostics,
            } => Self::ResourcesUpdate {
                skills: *skills,
                templates: *templates,
                diagnostics: diagnostics
                    .iter()
                    .map(SerializableSkillDiagnostic::from)
                    .collect(),
            },
            AgentHarnessEvent::SessionInfoUpdate { name } => {
                Self::SessionInfoUpdate { name: name.clone() }
            }
            AgentHarnessEvent::CompactionStart { estimated_tokens } => Self::CompactionStart {
                estimated_tokens: *estimated_tokens,
            },
            AgentHarnessEvent::CompactionEnd { stats, error } => Self::CompactionEnd {
                stats: stats.as_ref().map(SerializableCompactionStats::from),
                error: error.clone(),
            },
            AgentHarnessEvent::QueueUpdate {
                steer_len,
                follow_up_len,
            } => Self::QueueUpdate {
                steer_len: *steer_len,
                follow_up_len: *follow_up_len,
            },
            AgentHarnessEvent::SavePoint { entries_flushed } => Self::SavePoint {
                entries_flushed: *entries_flushed,
            },
            AgentHarnessEvent::BranchForked {
                from,
                new_leaf,
                label,
            } => Self::BranchForked {
                from: *from,
                new_leaf: *new_leaf,
                label: label.clone(),
            },
            AgentHarnessEvent::BranchSwitched { from, to } => Self::BranchSwitched {
                from: *from,
                to: *to,
            },
            AgentHarnessEvent::BranchDeleted { leaf } => Self::BranchDeleted { leaf: *leaf },
            AgentHarnessEvent::BranchSummarized { leaf, summary } => Self::BranchSummarized {
                leaf: *leaf,
                summary: summary.clone(),
            },
            AgentHarnessEvent::ToolCallStart {
                tool_use_id,
                tool_name,
                args,
            } => Self::ToolCallStart {
                tool_use_id: tool_use_id.clone(),
                tool_name: tool_name.clone(),
                args: args.clone(),
            },
            AgentHarnessEvent::ToolCallEnd {
                tool_use_id,
                tool_name,
                result,
            } => Self::ToolCallEnd {
                tool_use_id: tool_use_id.clone(),
                tool_name: tool_name.clone(),
                result: SerializableHarnessToolCallResult::from(result),
            },
            AgentHarnessEvent::Settled => Self::Settled,
            AgentHarnessEvent::Aborted => Self::Aborted,
        }
    }
}

/// Project a [`ThinkingLevel`] to a stable string name.
fn thinking_level_str(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "Off",
        ThinkingLevel::Minimal => "Minimal",
        ThinkingLevel::Low => "Low",
        ThinkingLevel::Medium => "Medium",
        ThinkingLevel::High => "High",
        ThinkingLevel::XHigh => "XHigh",
        ThinkingLevel::Budget(_) => "Budget",
    }
}

// ── SerializableHarnessState ─────────────────────────────────────────────────

/// Serializable projection of [`HarnessState`].
///
/// Drops: `model_info`, `response_format`, `default_execution_mode`,
/// `streaming_message`, `pending_tool_calls`, `pending_session_writes`,
/// `queued_next_turn`, `tools_overwritten_mid_run`.
///
/// `Vec<Arc<dyn Tool>>` is projected to `Vec<String>` (tool names).
#[derive(Debug, Clone, Serialize)]
pub struct SerializableHarnessState {
    pub phase: String,
    pub model: String,
    pub thinking_level: String,
    pub temperature: Option<f32>,
    pub tools: Vec<String>,
    pub active_tools: Option<Vec<String>>,
    pub final_answer_mode: String,
    pub system_prompt: Option<String>,
    pub error_message: Option<String>,
}

/// Project a [`HarnessState`] into its serializable form.
pub fn project_harness_state(state: &HarnessState) -> SerializableHarnessState {
    SerializableHarnessState {
        phase: phase_str(state.phase).to_string(),
        model: state.model.clone(),
        thinking_level: thinking_level_str(state.thinking_level).to_string(),
        temperature: state.temperature,
        tools: state.tools.iter().map(|t| t.name().to_string()).collect(),
        active_tools: state.active_tools.as_ref().map(|s| {
            let mut v: Vec<String> = s.iter().cloned().collect();
            v.sort();
            v
        }),
        final_answer_mode: format!("{:?}", state.final_answer_mode),
        system_prompt: state.system_prompt.clone(),
        error_message: state.error_message.clone(),
    }
}

// ── WsMessage ────────────────────────────────────────────────────────────────

/// WebSocket envelope message sent to Inspector clients.
///
/// For event messages, `type` is the `AgentHarnessEvent` variant name and
/// `inner_type` is the inner `AgentEvent` variant name (or `null` for
/// non-`Agent` variants). For the initial snapshot, `type` is `"Snapshot"`.
#[derive(Debug, Clone, Serialize)]
pub struct WsMessage {
    #[serde(serialize_with = "serialize_type_name")]
    pub r#type: &'static str,
    pub inner_type: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub data: serde_json::Value,
}

/// Serialize a `&'static str` as a plain JSON string (not an object key).
fn serialize_type_name<S: Serializer>(val: &str, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(val)
}

impl WsMessage {
    /// Build a `WsMessage` from a projected harness event.
    ///
    /// For `Agent { inner }` variants, `inner_type` is the `AgentEvent` variant
    /// name. All other variants get `inner_type: None`.
    pub fn from_event(event: &SerializableAgentHarnessEvent, timestamp: DateTime<Utc>) -> Self {
        let (type_name, inner_type) = match event {
            SerializableAgentHarnessEvent::Agent { inner } => {
                ("Agent", Some(agent_event_kind(inner)))
            }
            SerializableAgentHarnessEvent::PhaseChange { .. } => ("PhaseChange", None),
            SerializableAgentHarnessEvent::ModelUpdate { .. } => ("ModelUpdate", None),
            SerializableAgentHarnessEvent::ThinkingLevelUpdate { .. } => {
                ("ThinkingLevelUpdate", None)
            }
            SerializableAgentHarnessEvent::ToolsUpdate { .. } => ("ToolsUpdate", None),
            SerializableAgentHarnessEvent::ActiveToolsUpdate { .. } => ("ActiveToolsUpdate", None),
            SerializableAgentHarnessEvent::ResourcesUpdate { .. } => ("ResourcesUpdate", None),
            SerializableAgentHarnessEvent::SessionInfoUpdate { .. } => ("SessionInfoUpdate", None),
            SerializableAgentHarnessEvent::CompactionStart { .. } => ("CompactionStart", None),
            SerializableAgentHarnessEvent::CompactionEnd { .. } => ("CompactionEnd", None),
            SerializableAgentHarnessEvent::QueueUpdate { .. } => ("QueueUpdate", None),
            SerializableAgentHarnessEvent::SavePoint { .. } => ("SavePoint", None),
            SerializableAgentHarnessEvent::BranchForked { .. } => ("BranchForked", None),
            SerializableAgentHarnessEvent::BranchSwitched { .. } => ("BranchSwitched", None),
            SerializableAgentHarnessEvent::BranchDeleted { .. } => ("BranchDeleted", None),
            SerializableAgentHarnessEvent::BranchSummarized { .. } => ("BranchSummarized", None),
            SerializableAgentHarnessEvent::ToolCallStart { .. } => ("ToolCallStart", None),
            SerializableAgentHarnessEvent::ToolCallEnd { .. } => ("ToolCallEnd", None),
            SerializableAgentHarnessEvent::Settled => ("Settled", None),
            SerializableAgentHarnessEvent::Aborted => ("Aborted", None),
        };
        Self {
            r#type: type_name,
            inner_type: inner_type.map(|s| s.to_string()),
            timestamp,
            data: serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        }
    }

    /// Build the initial snapshot message.
    pub fn snapshot(
        state: &SerializableHarnessState,
        usage: &CostAggregate,
        timestamp: DateTime<Utc>,
    ) -> Self {
        let data = serde_json::json!({
            "state": state,
            "usage": usage,
        });
        Self {
            r#type: "Snapshot",
            inner_type: None,
            timestamp,
            data,
        }
    }

    /// Build the history-replay message: the session's active path projected
    /// into the same event shapes the live stream uses, so the UI renders
    /// it with the existing handleMessage pipeline.
    ///
    /// Only `Message` payloads are replayed — config-change entries
    /// (model/thinking/tools) are already reflected in the snapshot state.
    pub fn history(entries: &[llm_harness_agent::session::types::SessionEntry]) -> Self {
        use llm_harness_agent::session::types::SessionEntryPayload;
        use llm_harness_types::{AgentMessage, ContentBlock};

        let mut events: Vec<serde_json::Value> = Vec::new();
        let mut history_text = String::new();
        for entry in entries {
            let SessionEntryPayload::Message(msg) = &entry.payload else {
                continue;
            };
            match msg {
                AgentMessage::User(u) => {
                    for block in &u.content {
                        if let ContentBlock::Text { text } = block {
                            history_text.push_str("[user] ");
                            history_text.push_str(text.trim());
                            history_text.push('\n');
                        }
                    }
                }
                AgentMessage::Assistant(a) => {
                    for block in &a.content {
                        match block {
                            ContentBlock::Text { text } => {
                                history_text.push_str("[assistant] ");
                                history_text.push_str(text.trim());
                                history_text.push('\n');
                            }
                            ContentBlock::ToolUse { name, input, .. } => {
                                history_text.push_str(&format!(
                                    "[tool-call] {name} {}\n",
                                    serde_json::to_string(input).unwrap_or_else(|_| "{}".into())
                                ));
                            }
                            _ => {}
                        }
                    }
                }
                AgentMessage::ToolResult(t) => {
                    let texts: Vec<String> = t
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            llm_harness_types::DataBlock::Text { text, .. } => {
                                Some(text.trim().to_string())
                            }
                            _ => None,
                        })
                        .collect();
                    if !texts.is_empty() {
                        history_text.push_str(&format!(
                            "[tool-result{}] {}\n",
                            if t.is_error { " ERROR" } else { "" },
                            texts.join(" ")
                        ));
                    }
                }
                _ => {}
            }
        }
        events.push(serde_json::json!({ "history_text": history_text }));
        Self {
            r#type: "History",
            inner_type: None,
            timestamp: Utc::now(),
            data: serde_json::json!({ "messages": events }),
        }
    }
}

/// Extract the variant name (the `kind` tag value) from a `SerializableAgentEvent`.
fn agent_event_kind(e: &SerializableAgentEvent) -> &'static str {
    match e {
        SerializableAgentEvent::AgentStart => "AgentStart",
        SerializableAgentEvent::AgentEnd { .. } => "AgentEnd",
        SerializableAgentEvent::TurnStart { .. } => "TurnStart",
        SerializableAgentEvent::TurnEnd { .. } => "TurnEnd",
        SerializableAgentEvent::MessageStart { .. } => "MessageStart",
        SerializableAgentEvent::MessageUpdate { .. } => "MessageUpdate",
        SerializableAgentEvent::MessageEnd { .. } => "MessageEnd",
        SerializableAgentEvent::TextDelta { .. } => "TextDelta",
        SerializableAgentEvent::ThinkingDelta { .. } => "ThinkingDelta",
        SerializableAgentEvent::ToolCallStart { .. } => "ToolCallStart",
        SerializableAgentEvent::ToolCallArgsDelta { .. } => "ToolCallArgsDelta",
        SerializableAgentEvent::ToolCallEnd { .. } => "ToolCallEnd",
        SerializableAgentEvent::ToolExecutionStart { .. } => "ToolExecutionStart",
        SerializableAgentEvent::ToolExecutionUpdate { .. } => "ToolExecutionUpdate",
        SerializableAgentEvent::ToolExecutionEnd { .. } => "ToolExecutionEnd",
        SerializableAgentEvent::Error { .. } => "Error",
        SerializableAgentEvent::RetryAttempt { .. } => "RetryAttempt",
        SerializableAgentEvent::Unknown { .. } => "Unknown",
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::DataBlock;

    /// `AgentEvent::TextDelta` → `AgentHarnessEvent::Agent(...)` → JSON contains
    /// "TextDelta" and the text content.
    #[test]
    fn text_delta_serializes() {
        let agent_event = AgentEvent::TextDelta {
            message_id: "msg_1".to_string(),
            text: "hello world".to_string(),
        };
        let harness_event = AgentHarnessEvent::Agent(agent_event);
        let proj = SerializableAgentHarnessEvent::from(&harness_event);
        let json = serde_json::to_string(&proj).unwrap();

        assert!(
            json.contains("TextDelta"),
            "JSON should contain 'TextDelta'"
        );
        assert!(
            json.contains("hello world"),
            "JSON should contain the text content"
        );
    }

    /// `ToolResult::full(...)` → `SerializableToolResult` → JSON does NOT
    /// contain "extensions".
    #[test]
    fn tool_result_drops_extensions() {
        let result = ToolResult::full(
            vec![DataBlock::text("done")],
            serde_json::json!({"key": "val"}),
            false,
        );
        let proj = SerializableToolResult::from(&result);
        let json = serde_json::to_string(&proj).unwrap();

        assert!(
            !json.contains("extensions"),
            "JSON must NOT contain 'extensions'"
        );
        assert!(json.contains("model_content"));
        assert!(json.contains("done"));
    }

    /// `WsMessage::snapshot(&state, &usage)` → JSON has `"type":"Snapshot"`,
    /// `"inner_type":null`, `"data.state"`, `"data.usage"`.
    #[test]
    fn ws_message_snapshot_format() {
        let state = SerializableHarnessState {
            phase: "Idle".to_string(),
            model: "test-model".to_string(),
            thinking_level: "Off".to_string(),
            temperature: None,
            tools: vec![],
            active_tools: None,
            final_answer_mode: "Auto".to_string(),
            system_prompt: None,
            error_message: None,
        };
        let usage = CostAggregate::default();
        let ts = DateTime::parse_from_rfc3339("2026-08-22T10:30:05.123Z")
            .unwrap()
            .with_timezone(&Utc);
        let msg = WsMessage::snapshot(&state, &usage, ts);
        let json = serde_json::to_string(&msg).unwrap();

        assert!(
            json.contains(r#""type":"Snapshot""#),
            "type must be Snapshot"
        );
        assert!(
            json.contains(r#""inner_type":null"#),
            "inner_type must be null"
        );
        assert!(json.contains(r#""state""#), "data must contain state");
        assert!(json.contains(r#""usage""#), "data must contain usage");
    }

    /// `PhaseChange { Idle → Turning }` → JSON contains "PhaseChange", "Idle",
    /// "Turning".
    #[test]
    fn phase_change_serializes() {
        let event = AgentHarnessEvent::PhaseChange {
            from: HarnessPhase::Idle,
            to: HarnessPhase::Turning,
        };
        let proj = SerializableAgentHarnessEvent::from(&event);
        let json = serde_json::to_string(&proj).unwrap();

        assert!(json.contains("PhaseChange"));
        assert!(json.contains("Idle"));
        assert!(json.contains("Turning"));
    }

    /// `AgentEvent::Error(AgentError::Aborted)` → JSON contains "Error" and
    /// "aborted".
    #[test]
    fn agent_error_projects_to_string() {
        let agent_event = AgentEvent::Error(llm_harness_types::AgentError::Aborted);
        let harness_event = AgentHarnessEvent::Agent(agent_event);
        let proj = SerializableAgentHarnessEvent::from(&harness_event);
        let json = serde_json::to_string(&proj).unwrap();

        assert!(json.contains("Error"), "JSON should contain 'Error'");
        assert!(
            json.contains("aborted"),
            "JSON should contain the error display text 'aborted'"
        );
    }

    /// `WsMessage::from_event` extracts the correct `type` and `inner_type`.
    #[test]
    fn ws_message_from_event_extracts_types() {
        let agent_event = AgentEvent::TextDelta {
            message_id: "m1".to_string(),
            text: "hi".to_string(),
        };
        let harness_event = AgentHarnessEvent::Agent(agent_event);
        let proj = SerializableAgentHarnessEvent::from(&harness_event);
        let ts = Utc::now();
        let msg = WsMessage::from_event(&proj, ts);

        assert_eq!(msg.r#type, "Agent");
        assert_eq!(msg.inner_type.as_deref(), Some("TextDelta"));

        // Non-Agent variant: inner_type should be None
        let phase_event = AgentHarnessEvent::Settled;
        let proj2 = SerializableAgentHarnessEvent::from(&phase_event);
        let msg2 = WsMessage::from_event(&proj2, ts);
        assert_eq!(msg2.r#type, "Settled");
        assert!(msg2.inner_type.is_none());
    }

    /// `SerializableToolOutcome` handles both Ok and Err arms.
    #[test]
    fn tool_outcome_both_arms() {
        let ok = Result::<ToolResult, ToolFailure>::Ok(ToolResult::full(
            vec![DataBlock::text("ok")],
            serde_json::Value::Null,
            false,
        ));
        let proj = SerializableToolOutcome::from(&ok);
        let json = serde_json::to_string(&proj).unwrap();
        assert!(json.contains("model_content"));

        let err = Result::<ToolResult, ToolFailure>::Err(ToolFailure::aborted());
        let proj = SerializableToolOutcome::from(&err);
        let json = serde_json::to_string(&proj).unwrap();
        assert!(json.contains("aborted"));
    }
}
