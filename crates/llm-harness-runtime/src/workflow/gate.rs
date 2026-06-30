// crates/llm-harness-runtime/src/workflow/gate.rs
//! 流程门禁 hook。路径 1 下 LLM 物理上提不出越界，但保留 hook 作为兜底。
//!
//! 第一道闸：WorkflowGateHook（impl PrepareNextTurnHook）——主动收窄工具集。
//! 第二道闸：RuleBasedApprovalHook（来自 llm-harness-rules）——兜底拦截。
//! 两道闸问同一个 step_tools 映射。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_rules::{Contains, Decision, Rule, RuleBasedApprovalHook, RuleChain};
use llm_harness_types::{AgentError, NextTurnDirective, PrepareNextTurnCtx, PrepareNextTurnHook};
use tokio::sync::Mutex;

use crate::workflow::model::{StepId, WorkflowState};

/// 第一道闸：按当前步收窄工具集。实现 `PrepareNextTurnHook`。
///
/// 每 turn 前，读 `engine_state.current_step`，把 harness 工具集收窄到
/// 该步 `allowed_tools` ∪ `extra_tool_names`（引擎内置 tool 如 submit_step_result）。
/// 空 Vec = 不允许任何工具。
pub struct WorkflowGateHook {
    engine_state: Arc<Mutex<WorkflowState>>,
    step_tools: HashMap<StepId, Vec<String>>,
    /// 引擎每步注入的内置 tool 名（如 submit_step_result），不受 step.allowed_tools 限制。
    extra_tool_names: HashSet<String>,
}

impl WorkflowGateHook {
    pub fn new(
        engine_state: Arc<Mutex<WorkflowState>>,
        step_tools: HashMap<StepId, Vec<String>>,
        extra_tool_names: HashSet<String>,
    ) -> Self {
        Self { engine_state, step_tools, extra_tool_names }
    }
}

impl PrepareNextTurnHook for WorkflowGateHook {
    fn prepare<'a>(&'a self, _ctx: PrepareNextTurnCtx<'a>) -> BoxFuture<'a, Result<NextTurnDirective, AgentError>> {
        Box::pin(async move {
            let state = self.engine_state.lock().await;
            let allowed = self.step_tools.get(&state.current_step);
            // active_tools = step.allowed_tools ∪ extra_tool_names（内置 tool 不被门禁滤掉）。
            // 空 Vec = 不允许任何工具 → Some(空集 ∪ extra)（fail-secure）。
            // None 表示"激活全部"（见 hooks.rs），绝不能用于空 Vec。
            // 未知 current_step（step_tools.get 返回 None）同样 fail-secure 为 extra only。
            let mut active: HashSet<String> = allowed
                .map(|v| v.iter().cloned().collect())
                .unwrap_or_default();
            active.extend(self.extra_tool_names.iter().cloned());
            let directive = NextTurnDirective {
                context: None,
                model: None,
                thinking_level: None,
                tools: None,
                active_tools: Some(active),
            };
            Ok(directive)
        })
    }
}

/// 构建第二道闸（越界兜底）的 `RuleBasedApprovalHook`。
/// 规则：`tool_name ∈ 当前步 allowed_tools → Allow, fallback Deny`。
pub fn build_gate_approval_hook(allowed_tools: &[String]) -> Arc<RuleBasedApprovalHook> {
    let chain = RuleChain::builder()
        .rule(Rule {
            tool_name: "*".into(),
            predicate: Box::new(Contains(allowed_tools.to_vec())),
            on_match: Decision::Allow,
        })
        .fallback(Decision::Deny)
        .build();
    Arc::new(RuleBasedApprovalHook::new(chain))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn gate_returns_empty_set_when_empty_allowed() {
        // 空 Vec = 不允许任何工具 → Some(空集)，而非 None（None = 激活全部）。
        let state = Arc::new(Mutex::new(WorkflowState::new("s1".into())));
        let mut tools = HashMap::new();
        tools.insert("s1".to_string(), vec![]);
        let hook = WorkflowGateHook::new(state, tools, HashSet::new());
        let msg = make_empty_msg();
        let ctx = PrepareNextTurnCtx { turn_index: 0, last_message: &msg, last_tool_results: &[] };
        let d = hook.prepare(ctx).await.unwrap();
        let set = d.active_tools.expect("empty allowed must yield Some(empty set), not None");
        assert!(set.is_empty());
    }

    #[tokio::test]
    async fn gate_returns_empty_set_when_unknown_step() {
        // 未知 current_step（step_tools 无此 key）→ fail-secure 空集（仅含 extra）。
        let state = Arc::new(Mutex::new(WorkflowState::new("unknown".into())));
        let tools = HashMap::new();
        let hook = WorkflowGateHook::new(state, tools, HashSet::new());
        let msg = make_empty_msg();
        let ctx = PrepareNextTurnCtx { turn_index: 0, last_message: &msg, last_tool_results: &[] };
        let d = hook.prepare(ctx).await.unwrap();
        let set = d.active_tools.expect("unknown step must yield Some(empty set), not None");
        assert!(set.is_empty());
    }

    #[tokio::test]
    async fn gate_narrows_to_allowed_set() {
        let state = Arc::new(Mutex::new(WorkflowState::new("s1".into())));
        let mut tools = HashMap::new();
        tools.insert("s1".to_string(), vec!["tool_a".into(), "tool_b".into()]);
        let hook = WorkflowGateHook::new(state, tools, HashSet::new());
        let msg = make_empty_msg();
        let ctx = PrepareNextTurnCtx { turn_index: 0, last_message: &msg, last_tool_results: &[] };
        let d = hook.prepare(ctx).await.unwrap();
        let set = d.active_tools.unwrap();
        assert!(set.contains("tool_a"));
        assert!(set.contains("tool_b"));
        assert!(!set.contains("tool_c"));
    }

    #[tokio::test]
    async fn gate_unions_extra_tool_names() {
        // extra_tool_names（如 submit_step_result）始终在 active_tools 里，不受 allowed_tools 限制。
        let state = Arc::new(Mutex::new(WorkflowState::new("s1".into())));
        let mut tools = HashMap::new();
        tools.insert("s1".to_string(), vec!["tool_a".into()]);
        let extra: HashSet<String> = ["submit_step_result".into()].into_iter().collect();
        let hook = WorkflowGateHook::new(state, tools, extra);
        let msg = make_empty_msg();
        let ctx = PrepareNextTurnCtx { turn_index: 0, last_message: &msg, last_tool_results: &[] };
        let d = hook.prepare(ctx).await.unwrap();
        let set = d.active_tools.unwrap();
        assert!(set.contains("tool_a"));
        assert!(set.contains("submit_step_result"));
    }

    fn make_empty_msg() -> llm_harness_types::AssistantMessage {
        use chrono::Utc;
        llm_harness_types::AssistantMessage {
            content: vec![], stop_reason: None, timestamp: Utc::now(),
            provider: None, api: None, model: None, usage: None, error_message: None,
        }
    }
}
