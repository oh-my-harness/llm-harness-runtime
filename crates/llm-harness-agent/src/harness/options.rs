use std::sync::{Arc, Mutex, RwLock};

use llm_harness_loop::{ConvertToLlmHook, FinalAnswerMode, RetryConfig};
use llm_harness_types::*;

use crate::agent::ModelInfo;
use crate::compaction::CompactionModelConfig;
use crate::skills::{PromptTemplate, Skill};

use super::state::HarnessHooks;

const DEFAULT_MAX_TOKENS: u32 = 8192;
const DEFAULT_QUEUE_CAPACITY: usize = 32;
// ── Construction options ──────────────────────────────────────────────────────

/// Construction options for [`AgentHarness`].
pub struct AgentHarnessOptions {
    /// Initial model ID. Required.
    pub model: String,
    /// Optional model metadata.
    pub model_info: Option<ModelInfo>,
    /// Initial thinking level.
    pub thinking_level: ThinkingLevel,
    /// Initial sampling temperature; `None` uses provider default.
    pub temperature: Option<f32>,
    /// Optional `response_format` for structured output (e.g. `JsonObject`).
    pub response_format: Option<llm_harness_loop::ResponseFormat>,
    /// Initial tools.
    pub tools: Vec<Arc<dyn Tool>>,
    /// Final-answer classification mode.
    pub final_answer_mode: FinalAnswerMode,
    /// Initial system prompt.
    pub system_prompt: Option<String>,
    /// Maximum output tokens per LLM call.
    pub max_tokens: u32,
    /// mpsc channel capacity for steer/follow-up queues.
    pub queue_capacity: usize,
    /// Stream options for LLM requests.
    pub stream_options: StreamOptions,
    /// Hook collection (all optional).
    pub hooks: HarnessHooks,
    /// Custom context converter; uses `DefaultConvertToLlm` when `None`.
    pub convert_to_llm: Option<Arc<dyn ConvertToLlmHook>>,
    /// Pre-loaded skills.
    pub skills: Vec<Skill>,
    /// Shared readable-skills store for `SkillReadTool`. When `Some`,
    /// `reload_resources`/`add_skills` keep this in sync so the tool sees
    /// runtime skill updates. Created by `HarnessBuilder::inject_skill_read_tool`.
    pub readable_skills: Option<Arc<RwLock<Vec<Skill>>>>,
    /// Pre-loaded prompt templates.
    pub templates: Vec<PromptTemplate>,
    /// Retry config for transient provider errors; `None` disables retry.
    pub retry: Option<RetryConfig>,
    /// 共享成本状态（`Arc<Mutex<CostAggregate>>`）。由 builder 创建，
    /// 并注入共享同一 Arc 的 `CostAccumulatorHook` 到
    /// `hooks.after_provider_response`。默认为空的全新 aggregate。
    pub cost: Arc<Mutex<CostAggregate>>,
    /// Override tokens to reserve for the LLM response during compaction.
    pub compaction_reserve_tokens: Option<u32>,
    /// Override how many recent context tokens to always preserve during compaction.
    pub compaction_keep_recent_tokens: Option<u32>,
    /// Independent compaction provider/model. `None` → compaction uses the main
    /// conversation client/model. When `Some`, all three fields inside are set
    /// together — half-configuration is impossible at the type level.
    pub compaction_model: Option<CompactionModelConfig>,
    /// Enable automatic compaction at each turn start. Default: `false`.
    pub auto_compact: bool,
}

impl AgentHarnessOptions {
    /// Minimal options with only model set.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            model_info: None,
            thinking_level: ThinkingLevel::Off,
            temperature: None,
            response_format: None,
            tools: vec![],
            final_answer_mode: FinalAnswerMode::default(),
            system_prompt: None,
            max_tokens: DEFAULT_MAX_TOKENS,
            skills: vec![],
            readable_skills: None,
            stream_options: StreamOptions::default(),
            hooks: HarnessHooks::none(),
            convert_to_llm: None,
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            cost: Arc::new(Mutex::new(CostAggregate::default())),
            templates: vec![],
            retry: None,
            compaction_reserve_tokens: None,
            compaction_keep_recent_tokens: None,
            compaction_model: None,
            auto_compact: false,
        }
    }
}
