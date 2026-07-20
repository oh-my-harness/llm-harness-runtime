//! StructuredOutputCoordinator：声明式 structured 输出的审查 + 渐进升级。
//!
//! 三层防御 + 渐进升级策略：
//! 1. **Layer 1（prompt constraint）**：always —— 通过 [`system_prompt_segment`]
//!    在 system prompt 里追加"输出合法 JSON object"约束。
//! 2. **Layer 2（JsonObject upgrade）**：当 [`should_stop_hook`] 检测到 LLM 输出
//!    非合法 JSON 时，[`prepare_next_turn_hook`] 注入
//!    `response_format: Some(Some(ResponseFormat::JsonObject))`，升级下一轮。
//!    **仅在 provider 报告 `supports_json_schema=true` 时触发。**
//! 3. **Layer 3（give up）**：超过 `max_upgrade_attempts`（默认
//!    [`DEFAULT_MAX_UPGRADE_ATTEMPTS`] = 2，即最多 3 轮：初始 Turn 1 + 2 次升级）
//!    后，将状态置为 [`UpgradeState::Failed`]，上层据此设
//!    [`StructuredStatus::Failed`]。
//!
//! ## provider 支持矩阵
//!
//! - **OpenAI**：`capabilities.json_schema = true`，`to_oai` 转发 `response_format`
//!   → Layer 2 正常工作。
//! - **Anthropic**：`capabilities.json_schema = false`。即使 adapter 内部有合成工具
//!   `structured_result` 的实现（`anthropic/convert.rs:254`），该路径只在
//!   `chat_stream_native` + `AnthropicExtBuilder::structured_output` 下触发；
//!   runtime 走 `Provider::chat_stream`（`AnthropicExt::default()`，不读
//!   `req.response_format`），合成工具永不注入。因此 `supports_json_schema=false`
//!   → Layer 2 不可达，首次解析失败即直接走 Layer 3（Failed）。
//!   `extract_structured_from_message` 仅从文本提取,不从 ToolUse 块提取
//!   （P1-1: 此前有 Anthropic 合成工具提取分支,但无来源校验,LLM 可伪造
//!   `structured_result` ToolUse 绕过文本解析,已删除）。要真正支持 Anthropic
//!   structured 输出，需改 runtime 走 adapter native path（违反 spec §1.3
//!   约束）或改 adapter 在 Provider trait 转发 response_format。
//!
//! 协调器自身是一个共享状态机（`Arc<Mutex<UpgradeState>>`），由两个 hook 共享：
//! - [`StructuredShouldStopHook`]：每个 turn 结束时由 loop 调用，尝试从助手消息
//!   里提取 JSON object。提取成功 → 记录值并返回 `true`（停止循环）；提取失败
//!   → 推进升级状态机并返回 `false`（继续下一轮）。
//! - [`StructuredPrepareNextTurnHook`]：紧随其后被调用，根据当前升级状态决定
//!   是否在 `NextTurnDirective` 里注入 `response_format`，或在达上限时置 Failed。
//!
//! [`system_prompt_segment`]: StructuredOutputCoordinator::system_prompt_segment
//! [`should_stop_hook`]: StructuredOutputCoordinator::should_stop_hook
//! [`prepare_next_turn_hook`]: StructuredOutputCoordinator::prepare_next_turn_hook

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_loop::ResponseFormat;
use llm_harness_types::{
    AssistantMessage, NextTurnDirective, PrepareNextTurnCtx, PrepareNextTurnHook, ShouldStopCtx,
    ShouldStopHook,
};
use parking_lot::Mutex;
use serde_json::Value;

use super::json_parse::extract_json_object;
use crate::workflow::model::StructuredStatus;

/// 默认升级上限。= 2 意味着最多 3 轮：初始 Turn 1 + 2 次升级。
pub(crate) const DEFAULT_MAX_UPGRADE_ATTEMPTS: u32 = 2;

/// 追加到 system prompt 的结构化输出约束（Layer 1）。
pub(crate) const STRUCTURED_PROMPT_SEGMENT: &str = "\n\n# 输出格式要求\n你必须在最终回复里输出一个合法的 JSON object。不要在 JSON 前后添加解释、markdown 标记或任何多余文字。";

/// 协调器内部状态机。
///
/// 状态流转（happy path）：
/// ```text
/// Idle ──should_stop(JSON ok)──► JsonParsed
/// Idle ──should_stop(no JSON)──► UpgradeRequested{0} ──prepare──► Upgraded{1}
/// Upgraded{n} ──should_stop(JSON ok)──► JsonParsed
/// Upgraded{n} ──should_stop(no JSON)──► Upgraded{n} ──prepare──► Upgraded{n+1} 或 Failed
/// ```
#[derive(Debug, Clone)]
enum UpgradeState {
    /// 初始或刚被 `take_structured` 取走后的空状态。
    Idle,
    /// 成功解析出一个 JSON object，等待被 `take_structured` 取走。
    JsonParsed(Value),
    /// should_stop 检测到非 JSON，请求 prepare_next_turn 注入升级。`attempts`
    /// 是**已请求的升级次数**（prepare 会把它递增为已完成的升级次数）。
    UpgradeRequested { attempts: u32 },
    /// 已完成若干次升级（response_format 已注入），等待下一轮 should_stop 再判定。
    Upgraded { attempts: u32 },
    /// 超过 `max_upgrade_attempts` 或 provider 不支持 JsonObject，放弃。
    Failed,
}

/// 声明式 structured 输出的协调器。
///
/// 不可在线程间共享单实例 —— 而是通过 [`should_stop_hook`] / [`prepare_next_turn_hook`]
/// 派生出两个共享同一内部状态的 hook，分别挂到 `LoopConfig` 的 `should_stop`
/// 与 `prepare_next_turn` 上。两个 hook 之间通过 `Arc<Mutex<UpgradeState>>` 通信。
///
/// 典型用法：
/// ```ignore
/// let coord = StructuredOutputCoordinator::new(caps.supports_json_schema, DEFAULT_MAX_UPGRADE_ATTEMPTS);
/// let prompt = coord.system_prompt_segment();
/// loop_cfg.should_stop = Some(coord.should_stop_hook());
/// loop_cfg.prepare_next_turn = Some(coord.prepare_next_turn_hook());
/// // ... run loop ...
/// let status = coord.status();
/// let value = coord.take_structured();
/// ```
pub(crate) struct StructuredOutputCoordinator {
    state: Arc<Mutex<UpgradeState>>,
    supports_json_schema: bool,
    max_upgrade_attempts: u32,
}

impl StructuredOutputCoordinator {
    /// 创建协调器。
    ///
    /// - `supports_json_schema`：provider 是否支持 `ResponseFormat::JsonObject`。
    ///   `false` 时 Layer 2 永不注入 response_format，should_stop 一旦失败即直接
    ///   走 Layer 3（Failed）。仍会保留 Layer 1 的 prompt 约束。
    ///   **Anthropic 报告 `false`**（`capabilities.json_schema=false`），因此
    ///   Anthropic 上 Layer 2 不可达——即使 adapter 内部有合成工具实现，runtime
    ///   走 Provider trait 不触发它。详见模块文档"provider 支持矩阵"。
    /// - `max_upgrade_attempts`：允许的升级次数上限。到达后下一次 should_stop 失败
    ///   会直接置 Failed。
    pub(crate) fn new(supports_json_schema: bool, max_upgrade_attempts: u32) -> Self {
        Self {
            state: Arc::new(Mutex::new(UpgradeState::Idle)),
            supports_json_schema,
            max_upgrade_attempts,
        }
    }

    /// Layer 1：追加到 system prompt 的结构化输出约束。
    pub(crate) fn system_prompt_segment(&self) -> &'static str {
        STRUCTURED_PROMPT_SEGMENT
    }

    /// 派生 `ShouldStopHook`。每次 LLM 自然停止时由 loop 调用。
    pub(crate) fn should_stop_hook(&self) -> Arc<dyn ShouldStopHook> {
        Arc::new(StructuredShouldStopHook {
            state: self.state.clone(),
            max_upgrade_attempts: self.max_upgrade_attempts,
        })
    }

    /// 派生 `PrepareNextTurnHook`。should_stop 返回 `false` 后由 loop 调用。
    pub(crate) fn prepare_next_turn_hook(&self) -> Arc<dyn PrepareNextTurnHook> {
        Arc::new(StructuredPrepareNextTurnHook {
            state: self.state.clone(),
            supports_json_schema: self.supports_json_schema,
            max_upgrade_attempts: self.max_upgrade_attempts,
        })
    }

    /// 取走（并清空）已解析的 JSON 值。取走后状态回到 `Idle`。
    /// 仅在 [`status`] 返回 [`StructuredStatus::Ok`] 时才会返回 `Some`。
    ///
    /// [`status`]: StructuredOutputCoordinator::status
    pub(crate) fn take_structured(&self) -> Option<Value> {
        let mut state = self.state.lock();
        match std::mem::replace(&mut *state, UpgradeState::Idle) {
            UpgradeState::JsonParsed(v) => Some(v),
            _ => None,
        }
    }

    /// 当前结构化输出状态，映射到 [`StepResult::structured_status`]。
    ///
    /// [`StepResult::structured_status`]: crate::workflow::model::StepResult::structured_status
    pub(crate) fn status(&self) -> StructuredStatus {
        let state = self.state.lock();
        match *state {
            UpgradeState::JsonParsed(_) => StructuredStatus::Ok,
            _ => StructuredStatus::Failed,
        }
    }
}

/// `ShouldStopHook` 实现：尝试从助手消息提取 JSON，成功则停止，失败则推进升级。
struct StructuredShouldStopHook {
    state: Arc<Mutex<UpgradeState>>,
    max_upgrade_attempts: u32,
}
impl ShouldStopHook for StructuredShouldStopHook {
    fn should_stop<'a>(&'a self, ctx: ShouldStopCtx<'a>) -> BoxFuture<'a, bool> {
        let state = self.state.clone();
        let max_attempts = self.max_upgrade_attempts;
        Box::pin(async move {
            // 先尝试提取结构化结果。
            if let Some(value) = extract_structured_from_message(ctx.last_assistant) {
                *state.lock() = UpgradeState::JsonParsed(value);
                return true;
            }
            // 提取失败：推进升级状态机。
            let mut s = state.lock();
            let current_attempts = match &*s {
                UpgradeState::Idle => 0,
                UpgradeState::UpgradeRequested { attempts }
                | UpgradeState::Upgraded { attempts } => *attempts,
                // 已 Failed 或已 JsonParsed：不再继续循环。
                _ => return true,
            };
            if current_attempts == 0 {
                // 首次失败：请求升级。若 provider 不支持或 max=0 → 直接 Failed。
                // （provider 能力与上限的最终裁决在 prepare_next_turn，但 max=0
                //   时 prepare 也会 Failed；此处提前 Failed 避免多一次 LLM 调用。）
                if max_attempts == 0 {
                    *s = UpgradeState::Failed;
                    return true;
                }
                *s = UpgradeState::UpgradeRequested { attempts: 0 };
            } else if current_attempts >= max_attempts {
                // 已升级到上限仍未提取到 JSON：直接置 Failed 并停止，
                // 不再等 prepare_next_turn（避免多一次浪费的 LLM 调用）。
                *s = UpgradeState::Failed;
                return true;
            } else {
                // 已升级过但未达上限：保持 Upgraded 状态，让 prepare_next_turn
                // 决定是否再升一级（会重新注入 response_format）。
                *s = UpgradeState::Upgraded {
                    attempts: current_attempts,
                };
            }
            false
        })
    }
}

/// `PrepareNextTurnHook` 实现：根据升级状态注入 response_format 或置 Failed。
struct StructuredPrepareNextTurnHook {
    state: Arc<Mutex<UpgradeState>>,
    supports_json_schema: bool,
    max_upgrade_attempts: u32,
}

impl PrepareNextTurnHook for StructuredPrepareNextTurnHook {
    fn prepare<'a>(
        &'a self,
        _ctx: PrepareNextTurnCtx<'a>,
    ) -> BoxFuture<'a, Result<NextTurnDirective, llm_harness_types::AgentError>> {
        let state = self.state.clone();
        let supports = self.supports_json_schema;
        let max_attempts = self.max_upgrade_attempts;
        Box::pin(async move {
            let mut s = state.lock();
            let mut directive = NextTurnDirective {
                context: None,
                model: None,
                thinking_level: None,
                temperature: None,
                tools: None,
                active_tools: None,
                response_format: None,
            };
            match &*s {
                UpgradeState::UpgradeRequested { attempts } => {
                    let attempts = *attempts;
                    // 首次升级请求。若 provider 不支持或已达上限 → Failed。
                    if attempts >= max_attempts || !supports {
                        *s = UpgradeState::Failed;
                    } else {
                        *s = UpgradeState::Upgraded {
                            attempts: attempts + 1,
                        };
                        directive.response_format = Some(Some(ResponseFormat::JsonObject));
                    }
                }
                UpgradeState::Upgraded { attempts } => {
                    let attempts = *attempts;
                    // 已升级过仍未提取到 JSON。达上限 → Failed，否则再升一级。
                    //
                    // 注意 1：正常路径下 attempts >= max 不会到达此处 ——
                    // should_stop 已在上游检测到上限并直接置 Failed 返回 true。
                    // 此处保留作为防御性兜底（如 should_stop 未被调用、或状态被
                    // 外部置为 Upgraded{max} 的边缘场景）。
                    //
                    // 注意 2：未达上限时必须重新注入 response_format，因为
                    // DefaultPrepareNextTurn 的 base directive 会用
                    // HarnessState.response_format（恒 None）设 Some(None)（清除
                    // 语义）。若此处返回 None，composite 合并不会覆盖 base 的
                    // Some(None)，导致 JsonObject 被清除。
                    if attempts >= max_attempts {
                        *s = UpgradeState::Failed;
                    } else {
                        *s = UpgradeState::Upgraded {
                            attempts: attempts + 1,
                        };
                        directive.response_format = Some(Some(ResponseFormat::JsonObject));
                    }
                }
                // Idle / JsonParsed / Failed：不动 directive。
                _ => {}
            }
            Ok(directive)
        })
    }
}

/// 从助手消息里提取结构化结果。
///
/// 仅从文本内容提取,调用 [`extract_json_object`] 多层兜底解析。
///
/// **不**从 ToolUse 块提取。此前有一个 Anthropic 合成工具
/// `structured_result` 的提取分支,但:(1) 当前 Anthropic 配置下
/// (`supports_json_schema=false`)合成工具永不被注入,该分支是死代码;
/// (2) 无来源校验——LLM 可在 EndTurn 回合自发产出 `name="structured_result"`
/// 的 ToolUse 块伪造结构化结果,绕过文本 JSON 解析(P1-1 攻击面)。
/// 删除该分支消除攻击面。未来若修复 Anthropic 走 native path 使合成工具
/// 被注入,应在该路径单独处理提取(届时合成工具有可信来源)。
fn extract_structured_from_message(msg: &AssistantMessage) -> Option<Value> {
    extract_json_object(&msg.text_content())
}
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use llm_harness_types::{AssistantMessageKind, ContentBlock, StopReason};

    /// 构造只含文本块的助手消息。
    fn text_msg(text: &str) -> AssistantMessage {
        AssistantMessage {
            kind: AssistantMessageKind::FinalAnswer,
            message_id: "m1".into(),
            turn_id: "t1".into(),
            content: vec![ContentBlock::Text { text: text.into() }],
            stop_reason: Some(StopReason::EndTurn),
            timestamp: Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        }
    }

    fn mk_stop_ctx<'a>(msg: &'a AssistantMessage) -> ShouldStopCtx<'a> {
        ShouldStopCtx {
            last_assistant: msg,
            stop_reason: StopReason::EndTurn,
            turn_index: 0,
        }
    }

    /// 构造 `PrepareNextTurnCtx`，借用传入的消息。
    fn mk_prepare_ctx<'a>(msg: &'a AssistantMessage) -> PrepareNextTurnCtx<'a> {
        PrepareNextTurnCtx {
            turn_index: 0,
            last_message: msg,
            last_tool_results: &[],
        }
    }

    /// 用于 prepare 调用的占位空消息。
    fn placeholder_msg() -> AssistantMessage {
        text_msg("")
    }

    // ── Layer 1：system_prompt_segment ───────────────────────────────────────

    #[test]
    fn system_prompt_segment_is_nonempty_static() {
        let coord = StructuredOutputCoordinator::new(true, DEFAULT_MAX_UPGRADE_ATTEMPTS);
        let seg = coord.system_prompt_segment();
        assert!(!seg.is_empty());
        assert!(seg.contains("JSON"));
        // 同一实例多次调用返回同一 &'static str。
        assert!(std::ptr::eq(seg, coord.system_prompt_segment()));
    }

    // ── 解析成功：should_stop → JsonParsed，take_structured 取值 ──────────────

    #[tokio::test]
    async fn should_stop_returns_true_when_json_parsed() {
        let coord = StructuredOutputCoordinator::new(true, DEFAULT_MAX_UPGRADE_ATTEMPTS);
        let hook = coord.should_stop_hook();
        let msg = text_msg(r#"{"answer": 42}"#);
        let stop = hook.should_stop(mk_stop_ctx(&msg)).await;
        assert!(stop, "valid JSON object → should stop");
        assert_eq!(coord.status(), StructuredStatus::Ok);
        let v = coord.take_structured().expect("value present");
        assert_eq!(v["answer"], 42);
        // 取走后状态回到 Idle/Failed（非 JsonParsed）。
        assert_eq!(coord.status(), StructuredStatus::Failed);
        assert!(coord.take_structured().is_none());
    }

    #[tokio::test]
    async fn should_stop_parses_fenced_json_block() {
        let coord = StructuredOutputCoordinator::new(true, DEFAULT_MAX_UPGRADE_ATTEMPTS);
        let hook = coord.should_stop_hook();
        let msg = text_msg("Here you go:\n```json\n{\"k\": \"v\"}\n```\n");
        let stop = hook.should_stop(mk_stop_ctx(&msg)).await;
        assert!(stop);
        let v = coord.take_structured().unwrap();
        assert_eq!(v["k"], "v");
    }

    #[tokio::test]
    async fn should_stop_parses_json_with_surrounding_text() {
        let coord = StructuredOutputCoordinator::new(true, DEFAULT_MAX_UPGRADE_ATTEMPTS);
        let hook = coord.should_stop_hook();
        let msg = text_msg("Sure! {\"a\": 1} hope that helps");
        let stop = hook.should_stop(mk_stop_ctx(&msg)).await;
        assert!(stop);
        assert_eq!(coord.take_structured().unwrap()["a"], 1);
    }

    // ── Layer 2：解析失败 → 请求升级 → prepare 注入 response_format ──────────

    #[tokio::test]
    async fn prepare_injects_response_format_on_first_upgrade() {
        let coord = StructuredOutputCoordinator::new(true, DEFAULT_MAX_UPGRADE_ATTEMPTS);
        let stop = coord.should_stop_hook();
        let prep = coord.prepare_next_turn_hook();

        // 非法 JSON 文本。
        let msg = text_msg("I cannot do JSON, sorry");
        let should = stop.should_stop(mk_stop_ctx(&msg)).await;
        assert!(!should, "no JSON → continue");

        let directive = prep
            .prepare(mk_prepare_ctx(&placeholder_msg()))
            .await
            .unwrap();
        assert_eq!(
            directive.response_format,
            Some(Some(ResponseFormat::JsonObject)),
            "first upgrade must inject JsonObject"
        );
        assert_eq!(coord.status(), StructuredStatus::Failed);
    }

    #[tokio::test]
    async fn upgrade_then_success_recovers() {
        // 初始失败 → 升级 → 第二轮成功。
        let coord = StructuredOutputCoordinator::new(true, DEFAULT_MAX_UPGRADE_ATTEMPTS);
        let stop = coord.should_stop_hook();
        let prep = coord.prepare_next_turn_hook();

        let bad = text_msg("no json here");
        assert!(!stop.should_stop(mk_stop_ctx(&bad)).await);
        let d = prep
            .prepare(mk_prepare_ctx(&placeholder_msg()))
            .await
            .unwrap();
        assert_eq!(d.response_format, Some(Some(ResponseFormat::JsonObject)));

        // 第二轮：模型这次输出了合法 JSON。
        let good = text_msg(r#"{"ok": true}"#);
        assert!(stop.should_stop(mk_stop_ctx(&good)).await);
        assert_eq!(coord.status(), StructuredStatus::Ok);
        assert_eq!(coord.take_structured().unwrap()["ok"], true);
    }

    // ── provider 不支持 JsonObject → 跳过升级，直接 Failed ─────────────────

    #[tokio::test]
    async fn provider_without_json_schema_goes_straight_to_failed() {
        let coord = StructuredOutputCoordinator::new(false, DEFAULT_MAX_UPGRADE_ATTEMPTS);
        let stop = coord.should_stop_hook();
        let prep = coord.prepare_next_turn_hook();

        let bad = text_msg("no json");
        assert!(!stop.should_stop(mk_stop_ctx(&bad)).await);
        let d = prep
            .prepare(mk_prepare_ctx(&placeholder_msg()))
            .await
            .unwrap();
        assert!(
            d.response_format.is_none(),
            "provider does not support JsonObject → must not inject"
        );
        assert_eq!(coord.status(), StructuredStatus::Failed);
    }

    // ── Layer 3：达上限 → Failed ─────────────────────────────────────────────

    #[tokio::test]
    async fn max_upgrade_attempts_boundary() {
        // max = 2：初始 + 2 次升级 = 3 轮，第 3 轮仍失败 → Failed。
        let coord = StructuredOutputCoordinator::new(true, 2);
        let stop = coord.should_stop_hook();
        let prep = coord.prepare_next_turn_hook();
        let bad = text_msg("still no json");

        // Turn 1 失败 → 请求升级(0)。
        assert!(!stop.should_stop(mk_stop_ctx(&bad)).await);
        // prepare：attempts 0 < 2 且 supports → 升级到 Upgraded{1}，注入 fmt。
        let d1 = prep
            .prepare(mk_prepare_ctx(&placeholder_msg()))
            .await
            .unwrap();
        assert_eq!(d1.response_format, Some(Some(ResponseFormat::JsonObject)));

        // Turn 2 失败 → Upgraded{1}（保持）。
        assert!(!stop.should_stop(mk_stop_ctx(&bad)).await);
        // prepare：attempts 1 < 2 → 升级到 Upgraded{2}。Upgraded 分支必须重新注入
        // response_format，因为 DefaultPrepareNextTurn 的 base directive 会用
        // HarnessState.response_format（恒 None）设 Some(None)（清除语义）。
        let d2 = prep
            .prepare(mk_prepare_ctx(&placeholder_msg()))
            .await
            .unwrap();
        assert_eq!(
            d2.response_format,
            Some(Some(ResponseFormat::JsonObject)),
            "Upgraded branch must re-inject response_format (base directive would clear it)"
        );

        // Turn 3 失败 → should_stop 发现 Upgraded{2} 已达上限，直接置 Failed
        // 并返回 true（不再等 prepare_next_turn，避免多一次浪费的 LLM 调用）。
        let stop3 = stop.should_stop(mk_stop_ctx(&bad)).await;
        assert!(
            stop3,
            "attempts exhausted → should_stop returns true immediately"
        );
        assert_eq!(coord.status(), StructuredStatus::Failed);
    }

    #[tokio::test]
    async fn max_upgrade_attempts_zero_immediate_failed() {
        // max = 0：首次失败即 Failed，不升级。should_stop 直接置 Failed 并返回 true
        // （max==0 时无需进入 prepare_next_turn 判定，避免多一次 LLM 调用）。
        let coord = StructuredOutputCoordinator::new(true, 0);
        let stop = coord.should_stop_hook();
        let prep = coord.prepare_next_turn_hook();
        let bad = text_msg("nope");

        let stopped = stop.should_stop(mk_stop_ctx(&bad)).await;
        assert!(stopped, "max==0 → should_stop returns true immediately");
        assert_eq!(coord.status(), StructuredStatus::Failed);
        // prepare 不会被 loop 调用（should_stop 已返回 true），但直接调用应返回
        // 空 directive（Failed 状态下 prepare 不动 directive）。
        let d = prep
            .prepare(mk_prepare_ctx(&placeholder_msg()))
            .await
            .unwrap();
        assert!(d.response_format.is_none());
    }

    #[tokio::test]
    async fn failed_state_is_terminal_for_should_stop() {
        // 达 Failed 后再调 should_stop 直接返回 true（不再循环）。
        let coord = StructuredOutputCoordinator::new(true, 0);
        let stop = coord.should_stop_hook();
        let bad = text_msg("nope");

        // max==0 → 首次失败即 Failed。
        assert!(stop.should_stop(mk_stop_ctx(&bad)).await);
        assert_eq!(coord.status(), StructuredStatus::Failed);

        // 再次 should_stop：Failed 状态 → true（停止）。
        let again = stop.should_stop(mk_stop_ctx(&bad)).await;
        assert!(again, "Failed is terminal → should_stop returns true");
    }

    // ── 空输入边界 ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn empty_text_triggers_upgrade() {
        let coord = StructuredOutputCoordinator::new(true, DEFAULT_MAX_UPGRADE_ATTEMPTS);
        let stop = coord.should_stop_hook();
        let msg = text_msg("   ");
        let should = stop.should_stop(mk_stop_ctx(&msg)).await;
        assert!(!should, "empty text → no JSON → continue");
    }

    #[tokio::test]
    async fn json_array_is_not_accepted() {
        // extract_json_object 只接受 object；array 应判失败 → 升级。
        let coord = StructuredOutputCoordinator::new(true, DEFAULT_MAX_UPGRADE_ATTEMPTS);
        let stop = coord.should_stop_hook();
        let msg = text_msg("[1, 2, 3]");
        let should = stop.should_stop(mk_stop_ctx(&msg)).await;
        assert!(!should, "bare array is not a JSON object → upgrade");
    }

    // ── 默认常量 ─────────────────────────────────────────────────────────────

    #[test]
    fn default_max_upgrade_attempts_is_two() {
        assert_eq!(DEFAULT_MAX_UPGRADE_ATTEMPTS, 2);
    }

    // ── hook trait object 安全性 ─────────────────────────────────────────────

    #[test]
    fn hooks_are_dyn_compatible() {
        let coord = StructuredOutputCoordinator::new(true, DEFAULT_MAX_UPGRADE_ATTEMPTS);
        fn _accept_stop(_: &dyn ShouldStopHook) {}
        fn _accept_prep(_: &dyn PrepareNextTurnHook) {}
        _accept_stop(&*coord.should_stop_hook());
        _accept_prep(&*coord.prepare_next_turn_hook());
    }
}
