use std::sync::Arc;

use crate::{AgentMessage, Tool};

// ThinkingLevel 直接沿用 llm-api-adapter 的定义（单一来源）。
// adapter 的 ThinkingLevel 带 Copy/Eq/Serialize/Deserialize，满足 harness
// session 持久化与状态快照需求。
pub use llm_adapter::types::ThinkingLevel;

/// Agent loop 的输入上下文。
pub struct AgentContext {
    /// 系统提示；`None` 表示不设置系统提示。
    pub system_prompt: Option<String>,
    /// 消息历史列表。
    pub messages: Vec<AgentMessage>,
}

/// Turn 开始时的配置快照；turn 进行中对 Agent 的修改不影响当前 turn。
#[derive(Clone)]
pub struct TurnSnapshot {
    /// 使用的模型 ID。
    pub model: String,
    /// 推理深度级别。
    pub thinking_level: ThinkingLevel,
    /// 本 turn 激活的工具列表。
    pub tools: Vec<Arc<dyn Tool>>,
    /// 系统提示。
    pub system_prompt: Option<String>,
}

/// 传递给 LLM provider 的传输层配置；可被 `BeforeProviderRequestHook` 覆盖。
///
/// 当前 loop 会直接应用 `timeout_ms`、`max_retries` 和 `max_retry_delay_ms`。
/// 其余字段保留给支持 per-request 传输配置的 provider adapter。
#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    /// 请求超时（毫秒）；`None` 表示无超时。
    pub timeout_ms: Option<u64>,
    /// 最大重试次数；`None` 表示使用 provider 默认值。
    pub max_retries: Option<u32>,
    /// 重试最大延迟（毫秒）；`None` 表示使用 provider 默认值。
    pub max_retry_delay_ms: Option<u64>,
    /// 附加的 HTTP 请求头；需要 provider adapter 支持后才能透传。
    pub headers: Vec<(String, String)>,
    /// 厂商特定的元数据；需要 provider adapter 支持后才能透传。
    pub metadata: serde_json::Value,
    /// 厂商特定的缓存配置；需要 provider adapter 支持后才能透传。
    pub cache_config: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_level_copy() {
        let l = ThinkingLevel::High;
        let l2 = l;
        assert!(matches!(l2, ThinkingLevel::High));
    }

    #[test]
    fn agent_context_default_no_system_prompt() {
        let ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        assert!(ctx.system_prompt.is_none());
    }

    #[test]
    fn stream_options_default() {
        let opts = StreamOptions::default();
        assert!(opts.timeout_ms.is_none());
        assert!(opts.max_retries.is_none());
        assert!(opts.headers.is_empty());
    }
}
