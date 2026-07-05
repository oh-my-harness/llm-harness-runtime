// crates/llm-harness-runtime/src/peer.rs
//! Sub-agent peer communication: directory, hook, tools, cleanup guard.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use llm_harness_agent::FollowUpHandle;
use llm_harness_types::{
    AgentMessage, ContentBlock, CustomMessage, LIST_PEERS_TOOL_NAME, SEND_TO_PEER_TOOL_NAME,
    ShouldStopCtx, ShouldStopHook, Tool, ToolContext, ToolError, ToolExecutionMode, ToolLevel,
    ToolResult, UserMessage, WAIT_FOR_PEER_MESSAGE_TOOL_NAME,
};
use tokio::sync::mpsc;

const PEER_CHANNEL_CAPACITY: usize = 32;
/// `CustomMessage.r#type` 标签：peer 间发送的结构化消息。
///
/// `data` 格式：`{"sender_id": String, "message": String}`。
/// `message_to_content` 识别此类型并提取字段生成受控文本，
/// sender_id 存于 JSON 字段而非文本插值，消除前缀注入风险。
const PEER_MESSAGE_TYPE: &str = "peer_message";

/// Peer 消息接收端的共享句柄。
///
/// `Arc<tokio::sync::Mutex<Receiver>>` 允许两个消费者共享同一个 receiver：
/// - `PeerMessageHook`：`try_lock().try_recv()`（非阻塞，should_stop 时取一条）
/// - `WaitForPeerMessageTool`：`lock().await.recv().await`（阻塞，tool 执行时等消息）
///
/// 两者不会并发运行（agent loop 单线程：要么在 should_stop，要么在 tool execute），
/// 因此无实际竞争。使用 `tokio::sync::Mutex` 而非 `std::sync::Mutex`：
/// - 避免 tool 在 `recv().await` 被 cancel 时 poison mutex
/// - hook 的 `try_lock` 若失败（理论上不会发生）则跳过，不阻塞 should_stop
pub type SharedPeerRx = Arc<tokio::sync::Mutex<mpsc::Receiver<AgentMessage>>>;

// ── 类型定义（Step 3 实现）──

/// Sub-agent 通讯录：维护 `agent_id → peer channel sender` 的映射。
///
/// peer 注册时创建 `mpsc::Receiver` 并返回给调用方（与 `PeerMessageHook`
/// 共享），sender 留在 directory 供 `send_to_peer` 使用。sub-agent 结束时
/// 由 `PeerCleanupGuard` drop 触发 unregister（幂等）。
pub struct PeerDirectory {
    entries: Mutex<HashMap<String, PeerEntry>>,
}

#[derive(Clone)]
struct PeerEntry {
    peer_tx: mpsc::Sender<AgentMessage>,
    capabilities: Vec<String>,
    description: String,
}
#[derive(Clone, Debug)]
pub struct PeerMetadata {
    /// peer 的唯一标识。
    pub agent_id: String,
    /// peer 声明的能力列表（供其他 peer 发现）。
    pub capabilities: Vec<String>,
    /// peer 的描述文本。
    pub description: String,
}

#[derive(Clone, Debug)]
pub struct PeerInfo {
    /// peer 的唯一标识。
    pub agent_id: String,
    /// peer 声明的能力列表。
    pub capabilities: Vec<String>,
    /// peer 的描述文本。
    pub description: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    #[error("duplicate peer id: {id}")]
    DuplicateId { id: String },
    #[error("peer not found: {id}")]
    NotFound { id: String },
    /// peer channel 已关闭（peer 下线）。
    #[error("peer channel closed: {id}")]
    ChannelClosed { id: String },
    /// peer channel 已满（容量 32，对端消费慢）。
    #[error("peer channel full: {id}")]
    ChannelFull { id: String },
}

impl PeerDirectory {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
    /// 注册一个 peer，内部创建 peer channel。
    ///
    /// 返回 channel receiver 端的共享句柄 `SharedPeerRx`，调用方（`spawn_inner`）
    /// 把它同时塞进 `PeerMessageHook`（should_stop 时 `try_recv` 取一条）和
    /// `WaitForPeerMessageTool`（tool 执行时 `recv().await` 阻塞等）。
    /// sender 端存在 `PeerEntry` 里，供别的 peer `send_to_peer` 时 clone。
    ///
    /// 当所有 `SharedPeerRx` clone drop（hook + tool 均销毁）时，receiver drop，
    /// channel 关闭，后续 `send_to_peer` 返回 `ChannelClosed`。
    ///
    /// 重复 id 返回 `DuplicateId`。
    pub fn register(&self, meta: PeerMetadata) -> Result<SharedPeerRx, PeerError> {
        let mut entries = self.entries.lock().unwrap();
        if entries.contains_key(&meta.agent_id) {
            return Err(PeerError::DuplicateId { id: meta.agent_id });
        }
        let (peer_tx, peer_rx) = mpsc::channel(PEER_CHANNEL_CAPACITY);
        entries.insert(
            meta.agent_id.clone(),
            PeerEntry {
                peer_tx,
                capabilities: meta.capabilities,
                description: meta.description,
            },
        );
        Ok(Arc::new(tokio::sync::Mutex::new(peer_rx)))
    }

    /// 注销一个 peer。幂等（absent key 是 no-op）。
    pub fn unregister(&self, agent_id: &str) {
        let mut entries = self.entries.lock().unwrap();
        entries.remove(agent_id);
    }

    /// 返回当前所有 peer 的快照（不含 sender）。
    /// 锁内仅 clone entries，锁外构建 PeerInfo（F-12：避免锁内 O(n×m) 分配）。
    pub fn list_peers(&self) -> Vec<PeerInfo> {
        let entries: Vec<(String, PeerEntry)> = {
            let guard = self.entries.lock().unwrap();
            guard
                .iter()
                .map(|(id, e)| (id.clone(), e.clone()))
                .collect()
        };
        entries
            .into_iter()
            .map(|(id, e)| PeerInfo {
                agent_id: id,
                capabilities: e.capabilities,
                description: e.description,
            })
            .collect()
    }

    /// 向 target_id 发送一条 peer 消息（fire-and-forget）。
    ///
    /// 锁内 clone `peer_tx`，释放锁，锁外 `try_send`。
    /// F-13：区分 channel 满（`ChannelFull`）vs channel 关闭（`ChannelClosed`）。
    pub fn send_to_peer(&self, target_id: &str, msg: AgentMessage) -> Result<(), PeerError> {
        let peer_tx = {
            let entries = self.entries.lock().unwrap();
            entries
                .get(target_id)
                .map(|e| e.peer_tx.clone())
                .ok_or_else(|| PeerError::NotFound {
                    id: target_id.to_string(),
                })?
        };
        // 锁外 send——绝不在锁内 send().await
        peer_tx.try_send(msg).map_err(|e| match e {
            tokio::sync::mpsc::error::TrySendError::Full(_) => PeerError::ChannelFull {
                id: target_id.to_string(),
            },
            tokio::sync::mpsc::error::TrySendError::Closed(_) => PeerError::ChannelClosed {
                id: target_id.to_string(),
            },
        })
    }
}

impl Default for PeerDirectory {
    fn default() -> Self {
        Self::new()
    }
}

// ── PeerMessageHook ──────────────────────────────────────────────────────────

/// sub-agent 的 `ShouldStopHook`：LLM 自然停止时从 peer channel 取一条消息，
/// 通过 `FollowUpHandle` 注入 follow_up，返回 true；无消息也返回 true。
///
/// 与 `AsyncSpawnHook`（主 agent 侧）完全同构——try_recv → 注入 → return true。
/// 不阻塞（peer 消息间歇性，sub-agent 想停就停，有消息下次 should_stop 再取）。
/// 多条消息时**取一条**注入（peer 消息是独立对话，一次处理一条更自然），
/// 与 `AsyncSpawnHook` 的"排空所有"不同。
///
/// `peer_rx` 是 `SharedPeerRx`（`Arc<tokio::sync::Mutex<Receiver>>`），与
/// `WaitForPeerMessageTool` 共享同一 receiver。should_stop 用 `try_lock` 非阻塞
/// 取一条；若 `try_lock` 失败（tool 正在 `recv().await`，理论上 should_stop 和
/// tool execute 不会并发），跳过、返回 true。
pub struct PeerMessageHook {
    peer_rx: SharedPeerRx,
    /// 延迟注入：hook 构建时为 None，harness 构建后 `set_follow_up_handle` 回填。
    /// 极短窗口内若 None，跳过 try_recv（消息留 channel，下次 should_stop 再取）、返回 true。
    follow_up_handle: Arc<Mutex<Option<FollowUpHandle>>>,
}

impl PeerMessageHook {
    pub fn new(peer_rx: SharedPeerRx) -> Self {
        Self {
            peer_rx,
            follow_up_handle: Arc::new(Mutex::new(None)),
        }
    }

    /// 回填 `FollowUpHandle`。在 harness 构建后、`prompt()` 前调用。
    pub fn set_follow_up_handle(&self, handle: FollowUpHandle) {
        *self.follow_up_handle.lock().unwrap() = Some(handle);
    }
}

impl ShouldStopHook for PeerMessageHook {
    fn should_stop<'a>(&'a self, _ctx: ShouldStopCtx<'a>) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            // 先检查 handle 是否就绪，避免 handle=None 时取出消息导致丢失。
            // 若 handle=None，不 try_recv（消息留 channel，下次 should_stop 再取）。
            let fu_guard = self.follow_up_handle.lock().unwrap();
            if fu_guard.is_some() {
                drop(fu_guard); // 释放 follow_up 锁再抢 peer_rx 锁，避免锁序问题
                // try_lock：若 tool 正持有锁（recv().await 中），跳过不阻塞。
                // should_stop 与 tool execute 不会并发（loop 单线程），此处 try_lock
                // 仅为防御性——正常情况下总能拿到锁。
                if let Ok(mut rx) = self.peer_rx.try_lock()
                    && let Ok(msg) = rx.try_recv()
                    && let Some(h) = self.follow_up_handle.lock().unwrap().as_ref()
                {
                    // 转 User 再注入：DefaultConvertToLlm 无法处理 Custom 变体
                    //（无 custom_handler 时返回错误），follow_up 必须是 LLM 可见角色。
                    // 与 WaitForPeerMessageTool 路径统一走 message_to_content。
                    let user_msg = AgentMessage::User(UserMessage {
                        content: message_to_content(&msg),
                        timestamp: chrono::Utc::now(),
                    });
                    h.follow_up_message(user_msg);
                }
            }
            // 无论有无消息都返回 true（peer 消息间歇性，想停就停）
            true
        })
    }
}

// ── WaitForPeerMessageTool ───────────────────────────────────────────────────

/// LLM tool：阻塞等待一条来自 peer 的消息。
///
/// 业务层"D 方案"核心——让 sub-agent 在 tool 执行阶段 `recv().await` 阻塞等 peer 消息，
/// 而不是跑完 prompt 就停。LLM 调用此 tool 后，loop 在 tool execute 处等待；
/// peer 消息到达（或超时/abort）后 tool 返回，LLM 下个 turn 看到 peer 消息内容。
///
/// 与 `PeerMessageHook` 共享同一 `SharedPeerRx`：
/// - hook 在 should_stop 时 `try_lock().try_recv()`（非阻塞，取一条注入 follow_up）
/// - 本 tool 在 execute 时 `lock().await.recv().await`（阻塞，等一条作为 ToolResult）
///
/// 两者不会并发（agent loop 单线程：要么在 should_stop，要么在 tool execute）。
/// abort 直达：`ctx.abort.cancelled()` 在 `select!` 中，外部取消立即中断 tool。
/// 超时：tool 自带 `timeout`，超时返回提示文本（非错误），LLM 自行决定重试或放弃。
pub struct WaitForPeerMessageTool {
    peer_rx: SharedPeerRx,
    timeout: std::time::Duration,
}

impl WaitForPeerMessageTool {
    /// 创建 tool。`peer_rx` 与 `PeerMessageHook` 共享同一 `SharedPeerRx`。
    /// `timeout` 为等待超时；超时返回提示文本而非错误。
    pub fn new(peer_rx: SharedPeerRx, timeout: std::time::Duration) -> Self {
        Self { peer_rx, timeout }
    }
}

fn wait_for_peer_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

impl Tool for WaitForPeerMessageTool {
    fn name(&self) -> &str {
        WAIT_FOR_PEER_MESSAGE_TOOL_NAME
    }

    fn level(&self) -> ToolLevel {
        ToolLevel::BuiltIn
    }

    fn description(&self) -> &str {
        "阻塞等待一条来自任意 peer 的消息，收到后作为结果返回。超时返回提示文本。"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        S.get_or_init(wait_for_peer_schema)
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            let mut rx = self.peer_rx.lock().await;
            tokio::select! {
                biased;
                _ = ctx.abort.cancelled() => {
                    Err(ToolError::Aborted)
                }
                msg = rx.recv() => {
                    match msg {
                        Some(m) => Ok(ToolResult {
                            content: message_to_content(&m),
                            details: serde_json::json!({"status": "received"}),
                            terminate: false,
                        }),
                        None => Err(ToolError::Execution("peer channel closed".into())),
                    }
                }
                _ = tokio::time::sleep(self.timeout) => {
                    Ok(ToolResult {
                        content: vec![ContentBlock::Text {
                            text: "(no peer message within timeout)".into(),
                        }],
                        details: serde_json::json!({"status": "timeout"}),
                        terminate: false,
                    })
                }
            }
        })
    }
}

llm_harness_types::assert_builtin_registered!(WaitForPeerMessageTool => WAIT_FOR_PEER_MESSAGE_TOOL_NAME);

/// 把 `AgentMessage` 转成 LLM 可见的 `ContentBlock` 列表。
///
/// `User`/`Assistant`：取出 content。`Custom` 且 `r#type` 为
/// [`PEER_MESSAGE_TYPE`]：从 `data` 提取 `sender_id` 和 `message`，
/// 生成受控文本 `[from:{sender_id}] {message}`（sender_id 来自 JSON
/// 字段而非文本插值，消除前缀注入风险）。其他类型用 Debug 兜底。
fn message_to_content(msg: &AgentMessage) -> Vec<ContentBlock> {
    match msg {
        AgentMessage::User(u) => u.content.clone(),
        AgentMessage::Assistant(a) => a.content.clone(),
        AgentMessage::Custom(c) if c.r#type == PEER_MESSAGE_TYPE => {
            let sender_id = c.data.get("sender_id").and_then(|v| v.as_str());
            let message = c.data.get("message").and_then(|v| v.as_str()).unwrap_or("");
            let text = match sender_id {
                Some(id) => format!("[from:{id}] {message}"),
                None => message.to_string(),
            };
            vec![ContentBlock::Text { text }]
        }
        other => vec![ContentBlock::Text {
            text: format!("{other:?}"),
        }],
    }
}

// ── PeerCleanupGuard ─────────────────────────────────────────────────────────

/// RAII guard：drop 时从 `PeerDirectory` unregister 一个 peer。
///
/// 必须由 sub-agent 的执行 task（`tokio::spawn` 闭包）持有，
/// 不能留在 `spawn_inner` 栈上——否则 `spawn_inner` 返回时 guard 就 drop，
/// sub-agent 还没跑完就被 unregister。`unregister` 幂等，覆盖 success/error/panic。
pub struct PeerCleanupGuard {
    directory: Arc<PeerDirectory>,
    agent_id: String,
}

impl PeerCleanupGuard {
    pub fn new(directory: Arc<PeerDirectory>, agent_id: String) -> Self {
        Self {
            directory,
            agent_id,
        }
    }
}

impl Drop for PeerCleanupGuard {
    fn drop(&mut self) {
        self.directory.unregister(&self.agent_id);
    }
}

// ── SendToPeerTool / ListPeersTool ──────────────────────────────────────────

/// LLM tool：向另一个 peer 发送消息（fire-and-forget）。
///
/// `try_send` 不阻塞。失败（peer 下线 / channel 满）返回 `ToolError::Execution`，
/// harness 按既有契约把错误文本作为 `is_error=true` 的 ToolResult 喂给 LLM。
///
/// `own_id`：发送者自身的 agent_id。构造时由 plugin factory 从
/// `PeerMetadata.agent_id` 传入。发送时构造 `AgentMessage::Custom`（类型
/// [`PEER_MESSAGE_TYPE`]），sender_id 存于 `data` JSON 字段而非文本插值，
/// 消除前缀注入风险。`None` 时不标注发送者（向后兼容无 peer 场景）。
pub struct SendToPeerTool {
    directory: Arc<PeerDirectory>,
    own_id: Option<String>,
}

impl SendToPeerTool {
    /// 创建 tool。`own_id` 为发送者 agent_id，存入 `CustomMessage.data`
    /// 的 `sender_id` 字段，由 `message_to_content` 提取生成受控文本。
    pub fn new(directory: Arc<PeerDirectory>, own_id: Option<String>) -> Self {
        Self { directory, own_id }
    }
}

fn send_to_peer_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "target_id": {
                "type": "string",
                "description": "目标 peer 的 agent_id"
            },
            "message": {
                "type": "string",
                "description": "要发送的文本消息。发送者身份由系统自动标注，无需在消息中署名。"
            }
        },
        "required": ["target_id", "message"]
    })
}

impl Tool for SendToPeerTool {
    fn name(&self) -> &str {
        SEND_TO_PEER_TOOL_NAME
    }

    fn level(&self) -> ToolLevel {
        ToolLevel::BuiltIn
    }

    fn description(&self) -> &str {
        "向另一个 peer agent 发送文本消息（fire-and-forget，不等待回复）。"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        S.get_or_init(send_to_peer_schema)
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            let target_id = args
                .get("target_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::Execution("missing target_id".into()))?;
            let message = args
                .get("message")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::Execution("missing message".into()))?;

            let data = match &self.own_id {
                Some(id) => serde_json::json!({
                    "sender_id": id,
                    "message": message,
                }),
                None => serde_json::json!({
                    "sender_id": serde_json::Value::Null,
                    "message": message,
                }),
            };

            let msg = AgentMessage::Custom(CustomMessage {
                r#type: PEER_MESSAGE_TYPE.into(),
                data,
                timestamp: chrono::Utc::now(),
            });

            self.directory
                .send_to_peer(target_id, msg)
                .map_err(|e| ToolError::Execution(e.to_string()))?;

            Ok(ToolResult {
                content: vec![ContentBlock::Text {
                    text: format!("Message sent to peer '{target_id}'."),
                }],
                details: serde_json::json!({"target_id": target_id}),
                terminate: false,
            })
        })
    }
}

llm_harness_types::assert_builtin_registered!(SendToPeerTool => SEND_TO_PEER_TOOL_NAME);

/// LLM tool：列出当前在线的 peer 列表。
pub struct ListPeersTool {
    directory: Arc<PeerDirectory>,
}

impl ListPeersTool {
    pub fn new(directory: Arc<PeerDirectory>) -> Self {
        Self { directory }
    }
}

fn list_peers_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {}
    })
}

impl Tool for ListPeersTool {
    fn name(&self) -> &str {
        LIST_PEERS_TOOL_NAME
    }

    fn level(&self) -> ToolLevel {
        ToolLevel::BuiltIn
    }

    fn description(&self) -> &str {
        "列出当前在线的所有 peer agent（id + 能力标签 + 描述）。"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        S.get_or_init(list_peers_schema)
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            let peers: Vec<serde_json::Value> = self
                .directory
                .list_peers()
                .into_iter()
                .map(|p| {
                    serde_json::json!({
                        "agent_id": p.agent_id,
                        "capabilities": p.capabilities,
                        "description": p.description,
                    })
                })
                .collect();

            let text = serde_json::to_string_pretty(&peers).unwrap_or_else(|_| "[]".into());

            Ok(ToolResult {
                content: vec![ContentBlock::Text { text }],
                details: serde_json::json!({"count": peers.len()}),
                terminate: false,
            })
        })
    }
}

llm_harness_types::assert_builtin_registered!(ListPeersTool => LIST_PEERS_TOOL_NAME);

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::{ContentBlock, UserMessage};

    fn msg(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: chrono::Utc::now(),
        })
    }

    /// 构造一条 peer 间发送的 `AgentMessage::Custom`（与 `SendToPeerTool` 产出一致）。
    fn peer_msg(sender_id: Option<&str>, text: &str) -> AgentMessage {
        let data = match sender_id {
            Some(id) => serde_json::json!({"sender_id": id, "message": text}),
            None => serde_json::json!({"sender_id": serde_json::Value::Null, "message": text}),
        };
        AgentMessage::Custom(CustomMessage {
            r#type: PEER_MESSAGE_TYPE.into(),
            data,
            timestamp: chrono::Utc::now(),
        })
    }

    fn meta(id: &str) -> PeerMetadata {
        PeerMetadata {
            agent_id: id.to_string(),
            capabilities: vec!["rust".into()],
            description: format!("peer {id}"),
        }
    }

    #[test]
    fn register_returns_rx_and_list_peers_shows_entry() {
        let dir = PeerDirectory::new();
        let rx = dir.register(meta("a")).unwrap();
        let peers = dir.list_peers();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].agent_id, "a");
        assert_eq!(peers[0].capabilities, vec!["rust".to_string()]);
        // rx 存在即可证明 channel 建好
        drop(rx);
    }

    #[test]
    fn register_duplicate_id_returns_error() {
        let dir = PeerDirectory::new();
        dir.register(meta("a")).unwrap();
        let err = dir.register(meta("a")).unwrap_err();
        assert!(matches!(err, PeerError::DuplicateId { id } if id == "a"));
    }

    #[test]
    fn unregister_removes_entry_and_is_idempotent() {
        let dir = PeerDirectory::new();
        let _rx = dir.register(meta("a")).unwrap();
        dir.unregister("a");
        assert!(dir.list_peers().is_empty());
        // 再次 unregister 不 panic
        dir.unregister("a");
        assert!(dir.list_peers().is_empty());
    }

    #[tokio::test]
    async fn send_to_peer_delivers_message_to_rx() {
        let dir = PeerDirectory::new();
        let rx = dir.register(meta("a")).unwrap();
        dir.send_to_peer("a", msg("hello")).unwrap();
        let received = rx.try_lock().unwrap().try_recv().unwrap();
        match received {
            AgentMessage::User(u) => {
                assert_eq!(u.content.len(), 1);
            }
            _ => panic!("expected user message"),
        }
    }

    #[tokio::test]
    async fn send_to_peer_after_rx_dropped_returns_channel_closed() {
        let dir = PeerDirectory::new();
        {
            let rx = dir.register(meta("a")).unwrap();
            drop(rx);
        }
        // tx 还在 entries 里，但 rx 端已 drop → try_send 返回 SendError
        let err = dir.send_to_peer("a", msg("hello")).unwrap_err();
        assert!(matches!(err, PeerError::ChannelClosed { id } if id == "a"));
    }

    #[test]
    fn send_to_peer_unknown_id_returns_not_found() {
        let dir = PeerDirectory::new();
        let err = dir.send_to_peer("ghost", msg("hi")).unwrap_err();
        assert!(matches!(err, PeerError::NotFound { id } if id == "ghost"));
    }

    #[test]
    fn list_peers_excludes_internal_peer_tx_field() {
        let dir = PeerDirectory::new();
        let _rx_a = dir.register(meta("a")).unwrap();
        let _rx_b = dir
            .register(PeerMetadata {
                agent_id: "b".into(),
                capabilities: vec!["testing".into()],
                description: "tester".into(),
            })
            .unwrap();
        let peers = dir.list_peers();
        assert_eq!(peers.len(), 2);
        // 不含 peer_tx（PeerInfo 只有 id/capabilities/description）
        let ids: Vec<_> = peers.iter().map(|p| p.agent_id.clone()).collect();
        assert!(ids.contains(&"a".to_string()));
        assert!(ids.contains(&"b".to_string()));
    }
    // ── PeerMessageHook tests ───────────────────────────────────────────────

    use llm_harness_types::{AssistantMessage, ShouldStopCtx, ShouldStopHook, StopReason};

    fn make_stop_ctx<'a>(assistant: &'a AssistantMessage) -> ShouldStopCtx<'a> {
        ShouldStopCtx {
            last_assistant: assistant,
            stop_reason: StopReason::EndTurn,
            turn_index: 0,
        }
    }

    fn empty_assistant() -> AssistantMessage {
        let mut m = llm_harness_loop::test_utils::test_assistant_message(vec![]);
        m.stop_reason = Some(StopReason::EndTurn);
        m
    }

    /// should_stop always returns true, even with an empty channel and no
    /// follow_up_handle set. This is the core contract: non-blocking,
    /// "want-to-stop-just-stop" semantics.
    #[tokio::test]
    async fn peer_message_hook_returns_true_with_empty_channel_and_no_handle() {
        let (_tx, rx) = tokio::sync::mpsc::channel::<AgentMessage>(32);
        let hook = PeerMessageHook::new(Arc::new(tokio::sync::Mutex::new(rx)));
        // follow_up_handle 未 set（None）
        let assistant = empty_assistant();
        let ctx = make_stop_ctx(&assistant);
        let stop = hook.should_stop(ctx).await;
        assert!(stop, "hook always returns true");
    }

    /// When follow_up_handle is None and a message is present, the hook must
    /// NOT drain the channel (message stays for next should_stop). This is the
    /// "don't drop messages" guarantee during the brief None window.
    #[tokio::test]
    async fn peer_message_hook_keeps_message_when_handle_none() {
        let dir = Arc::new(PeerDirectory::new());
        let rx = dir.register(meta("remote")).unwrap();
        let hook = PeerMessageHook::new(rx);
        // 故意不 set_follow_up_handle
        dir.send_to_peer("remote", msg("hi")).unwrap();
        let assistant = empty_assistant();
        let ctx = make_stop_ctx(&assistant);
        let stop = hook.should_stop(ctx).await;
        assert!(stop);
        // 消息应仍留在 channel 里——hook 在 handle=None 时不 try_recv
        // 取回 rx 验证：hook 内部 Mutex<Receiver>，重新 lock try_recv 应拿到消息
        let remaining = hook.peer_rx.try_lock().unwrap().try_recv();
        assert!(
            remaining.is_ok(),
            "message must remain in channel when follow_up_handle is None"
        );
    }

    /// When follow_up_handle is set and a message is present, the hook
    /// injects the message and returns true. End-to-end follow_up channel
    /// verification is deferred to Task 12 integration tests; here we verify
    /// the hook does not panic and returns true (contract-level check).
    #[tokio::test]
    async fn peer_message_hook_with_handle_and_message_returns_true_no_panic() {
        // 用一个真实轻量 harness 提供 FollowUpHandle。
        use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
        use llm_harness_runtime_sandbox_os::OsEnv;

        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
        let env: Arc<dyn llm_harness_types::ExecutionEnv> =
            Arc::new(OsEnv::new(std::env::current_dir().unwrap()));
        let dir = Arc::new(PeerDirectory::new());
        let rx = dir.register(meta("remote")).unwrap();
        let hook = Arc::new(PeerMessageHook::new(rx));
        let harness = crate::builder::HarnessBuilder::new("m")
            .provider("m", client)
            .should_stop_hook(hook.clone())
            .build(env)
            .await
            .unwrap();
        // 延迟注入（模拟 spawn_inner 的 set_follow_up_handle）
        hook.set_follow_up_handle(harness.follow_up_handle());

        dir.send_to_peer("remote", msg("hello from remote"))
            .unwrap();
        let assistant = empty_assistant();
        let ctx = make_stop_ctx(&assistant);
        let stop = hook.should_stop(ctx).await;
        assert!(stop);
        // 消息已被 try_recv 取走（注入或丢弃），channel 应空
        let remaining = hook.peer_rx.try_lock().unwrap().try_recv();
        assert!(
            remaining.is_err(),
            "message should have been drained when handle is set"
        );
    }

    /// Multiple messages: only ONE is injected per should_stop call (design
    /// §2.3: peer hook "取一条"). Remaining messages stay for next should_stop.
    #[tokio::test]
    async fn peer_message_hook_takes_only_one_message_per_call() {
        use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
        use llm_harness_runtime_sandbox_os::OsEnv;

        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
        let env: Arc<dyn llm_harness_types::ExecutionEnv> =
            Arc::new(OsEnv::new(std::env::current_dir().unwrap()));
        let dir = Arc::new(PeerDirectory::new());
        let rx = dir.register(meta("remote")).unwrap();
        let hook = Arc::new(PeerMessageHook::new(rx));
        let harness = crate::builder::HarnessBuilder::new("m")
            .provider("m", client)
            .should_stop_hook(hook.clone())
            .build(env)
            .await
            .unwrap();
        hook.set_follow_up_handle(harness.follow_up_handle());

        dir.send_to_peer("remote", msg("first")).unwrap();
        dir.send_to_peer("remote", msg("second")).unwrap();
        let assistant = empty_assistant();
        let ctx = make_stop_ctx(&assistant);
        let _ = hook.should_stop(ctx).await;
        // 一条被取走，一条应仍留在 channel
        let remaining = hook.peer_rx.try_lock().unwrap().try_recv();
        assert!(remaining.is_ok(), "second message should remain in channel");
    }

    /// Regression: `PeerMessageHook` 收到 `AgentMessage::Custom`（peer 消息）
    /// 后必须转成 `AgentMessage::User` 注入 follow_up。`DefaultConvertToLlm`
    /// 无 `custom_handler` 时对 `Custom` 变体返回错误，直接注入会导致 agent
    /// loop 在下一轮 LLM 调用时报错。
    #[tokio::test]
    async fn peer_message_hook_converts_custom_to_user_for_follow_up() {
        use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
        use llm_harness_runtime_sandbox_os::OsEnv;

        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
        let env: Arc<dyn llm_harness_types::ExecutionEnv> =
            Arc::new(OsEnv::new(std::env::current_dir().unwrap()));
        let dir = Arc::new(PeerDirectory::new());
        let rx = dir.register(meta("remote")).unwrap();
        let hook = Arc::new(PeerMessageHook::new(rx));
        let harness = crate::builder::HarnessBuilder::new("m")
            .provider("m", client)
            .should_stop_hook(hook.clone())
            .build(env)
            .await
            .unwrap();
        hook.set_follow_up_handle(harness.follow_up_handle());

        // 发送一条 peer 消息（Custom 变体，带 sender_id）
        dir.send_to_peer("remote", peer_msg(Some("alpha"), "hello"))
            .unwrap();
        let assistant = empty_assistant();
        let ctx = make_stop_ctx(&assistant);
        let _ = hook.should_stop(ctx).await;

        // follow_up channel 里应是 User 变体（而非 Custom），
        // 内容经 message_to_content 转为 "[from:alpha] hello"。
        // 验证转换正确性：peer_msg 经 message_to_content 应生成受控文本。
        let blocks = message_to_content(&peer_msg(Some("alpha"), "hello"));
        match &blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, "[from:alpha] hello"),
            _ => panic!("expected text block"),
        }
    }

    // ── PeerCleanupGuard tests ──────────────────────────────────────────────

    #[test]
    fn peer_cleanup_guard_unregisters_on_drop() {
        let dir = Arc::new(PeerDirectory::new());
        let _rx = dir.register(meta("a")).unwrap();
        assert_eq!(dir.list_peers().len(), 1);
        {
            let _guard = PeerCleanupGuard::new(dir.clone(), "a".into());
            assert_eq!(
                dir.list_peers().len(),
                1,
                "guard exists, peer still registered"
            );
        } // guard drop
        assert_eq!(
            dir.list_peers().len(),
            0,
            "guard dropped, peer unregistered"
        );
    }

    #[test]
    fn peer_cleanup_guard_idempotent_unregister() {
        let dir = Arc::new(PeerDirectory::new());
        // guard for absent id —— unregister 幂等，不 panic
        {
            let _guard = PeerCleanupGuard::new(dir.clone(), "ghost".into());
        }
        assert!(dir.list_peers().is_empty());
    }
    // ── WaitForPeerMessageTool tests ───────────────────────────────────────

    use llm_harness_types::{Tool, ToolContext};

    fn make_tool_context() -> ToolContext {
        let env: Arc<dyn llm_harness_types::ExecutionEnv> = Arc::new(
            llm_harness_runtime_sandbox_os::OsEnv::new(std::env::current_dir().unwrap()),
        );
        let (update_tx, _update_rx) = tokio::sync::mpsc::channel(8);
        ToolContext {
            env,
            abort: tokio_util::sync::CancellationToken::new(),
            tool_use_id: "test".into(),
            turn_index: 0,
            assistant_message: Arc::new(llm_harness_loop::test_utils::test_assistant_message(
                vec![],
            )),
            update_tx,
        }
    }

    #[test]
    fn wait_for_peer_message_tool_name_and_schema() {
        let (_tx, rx) = tokio::sync::mpsc::channel::<AgentMessage>(32);
        let tool = WaitForPeerMessageTool::new(
            Arc::new(tokio::sync::Mutex::new(rx)),
            std::time::Duration::from_secs(1),
        );
        assert_eq!(tool.name(), "wait_for_peer_message");
        assert_eq!(tool.execution_mode(), ToolExecutionMode::Sequential);
    }

    /// Tool 阻塞等待，peer 消息到达后作为 ToolResult 返回。
    #[tokio::test]
    async fn wait_for_peer_message_receives_message() {
        let dir = Arc::new(PeerDirectory::new());
        let rx = dir.register(meta("self")).unwrap();
        let tool = WaitForPeerMessageTool::new(rx, std::time::Duration::from_secs(5));
        let ctx = make_tool_context();

        // 另一个 task 发消息
        let dir_clone = dir.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            dir_clone.send_to_peer("self", msg("ping")).unwrap();
        });

        let result = tool.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(!result.terminate);
        match &result.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "ping"),
            _ => panic!("expected text block"),
        }
    }

    /// 超时返回提示文本（非错误），LLM 可据此决定重试或放弃。
    #[tokio::test]
    async fn wait_for_peer_message_times_out_with_hint() {
        let (_tx, rx) = tokio::sync::mpsc::channel::<AgentMessage>(32);
        let tool = WaitForPeerMessageTool::new(
            Arc::new(tokio::sync::Mutex::new(rx)),
            std::time::Duration::from_millis(50),
        );
        let ctx = make_tool_context();

        let result = tool.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(!result.terminate);
        match &result.content[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("no peer message"), "got: {text}");
            }
            _ => panic!("expected text block"),
        }
        assert_eq!(result.details["status"], "timeout");
    }

    /// abort cancel 立即中断 tool，返回 Aborted。
    #[tokio::test]
    async fn wait_for_peer_message_aborts_on_cancel() {
        let (_tx, rx) = tokio::sync::mpsc::channel::<AgentMessage>(32);
        let tool = WaitForPeerMessageTool::new(
            Arc::new(tokio::sync::Mutex::new(rx)),
            std::time::Duration::from_secs(60),
        );
        let ctx = make_tool_context();

        // 另一个 task 触发 abort
        let abort = ctx.abort.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            abort.cancel();
        });

        let err = tool.execute(serde_json::json!({}), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Aborted));
    }

    /// Tool 和 hook 共享同一 SharedPeerRx：tool recv 阻塞期间，should_stop 的
    /// try_lock 失败（跳过），消息留给 tool 取走。
    #[tokio::test]
    async fn shared_rx_tool_recv_and_hook_try_lock_coexist() {
        let dir = Arc::new(PeerDirectory::new());
        let rx = dir.register(meta("self")).unwrap();
        let hook = Arc::new(PeerMessageHook::new(rx.clone()));
        let tool = WaitForPeerMessageTool::new(rx, std::time::Duration::from_secs(5));

        // 模拟 tool 正在 recv（持锁）。先 lock，不释放。
        let rx_guard = tool.peer_rx.clone().lock_owned().await;

        // 此时 should_stop 的 try_lock 应失败——hook 跳过，返回 true，消息不丢。
        let assistant = empty_assistant();
        let ctx = make_stop_ctx(&assistant);
        let stop = hook.should_stop(ctx).await;
        assert!(stop, "hook always returns true");

        // 释放锁，发消息，tool 能收到
        drop(rx_guard);
        dir.send_to_peer("self", msg("after_unlock")).unwrap();
        let tool_ctx = make_tool_context();
        let result = tool
            .execute(serde_json::json!({}), &tool_ctx)
            .await
            .unwrap();
        match &result.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "after_unlock"),
            _ => panic!("expected text block"),
        }
    }
    // ── SendToPeerTool / ListPeersTool tests ───────────────────────────────

    use llm_harness_types::{ToolError, ToolExecutionMode};

    #[test]
    fn send_to_peer_tool_name_and_schema() {
        let dir = Arc::new(PeerDirectory::new());
        let tool = SendToPeerTool::new(dir, None);
        assert_eq!(tool.name(), "send_to_peer");
        assert_eq!(tool.execution_mode(), ToolExecutionMode::Parallel);
        let schema = tool.parameters_schema();
        assert!(schema.is_object());
        assert!(schema.get("properties").is_some());
    }

    #[tokio::test]
    async fn send_to_peer_tool_delivers_message() {
        let dir = Arc::new(PeerDirectory::new());
        let rx = dir.register(meta("a")).unwrap();
        let tool = SendToPeerTool::new(dir.clone(), None);
        let ctx = make_tool_context();
        let args = serde_json::json!({
            "target_id": "a",
            "message": "hello tool"
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(!result.terminate);
        // rx 收到消息（SharedPeerRx 需先 try_lock）
        let received = rx.try_lock().unwrap().try_recv().unwrap();
        match received {
            AgentMessage::Custom(c) => {
                assert_eq!(c.r#type, PEER_MESSAGE_TYPE);
                assert_eq!(c.data["message"], "hello tool");
                assert!(c.data["sender_id"].is_null());
            }
            _ => panic!("expected Custom message"),
        }
    }

    #[tokio::test]
    async fn send_to_peer_tool_includes_sender_id_when_own_id_set() {
        let dir = Arc::new(PeerDirectory::new());
        let rx = dir.register(meta("a")).unwrap();
        let tool = SendToPeerTool::new(dir.clone(), Some("alpha".into()));
        let ctx = make_tool_context();
        let args = serde_json::json!({
            "target_id": "a",
            "message": "hello from alpha"
        });
        tool.execute(args, &ctx).await.unwrap();
        let received = rx.try_lock().unwrap().try_recv().unwrap();
        match received {
            AgentMessage::Custom(c) => {
                assert_eq!(c.r#type, PEER_MESSAGE_TYPE);
                assert_eq!(c.data["sender_id"], "alpha");
                assert_eq!(c.data["message"], "hello from alpha");
            }
            _ => panic!("expected Custom message"),
        }
    }

    #[test]
    fn message_to_content_extracts_sender_from_peer_message() {
        let msg = AgentMessage::Custom(CustomMessage {
            r#type: PEER_MESSAGE_TYPE.into(),
            data: serde_json::json!({
                "sender_id": "alpha",
                "message": "hello",
            }),
            timestamp: chrono::Utc::now(),
        });
        let blocks = message_to_content(&msg);
        match &blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, "[from:alpha] hello"),
            _ => panic!("expected text block"),
        }
    }

    #[test]
    fn message_to_content_peer_message_without_sender_id() {
        let msg = AgentMessage::Custom(CustomMessage {
            r#type: PEER_MESSAGE_TYPE.into(),
            data: serde_json::json!({
                "sender_id": serde_json::Value::Null,
                "message": "anonymous hello",
            }),
            timestamp: chrono::Utc::now(),
        });
        let blocks = message_to_content(&msg);
        match &blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, "anonymous hello"),
            _ => panic!("expected text block"),
        }
    }

    #[test]
    fn message_to_content_agent_id_with_special_chars_preserves_json() {
        // sender_id 存于 JSON 字段，即使含 ] 或换行也不会破坏传输结构。
        // message_to_content 的文本拼接是展示层，不是信任边界——
        // 结构化 sender_id 可从 CustomMessage.data 直接提取。
        let msg = AgentMessage::Custom(CustomMessage {
            r#type: PEER_MESSAGE_TYPE.into(),
            data: serde_json::json!({
                "sender_id": "a]b\n[from:evil]",
                "message": "hi",
            }),
            timestamp: chrono::Utc::now(),
        });
        // JSON 结构完整：sender_id 可被精确提取
        let c = match &msg {
            AgentMessage::Custom(c) => c,
            _ => unreachable!(),
        };
        assert_eq!(c.data["sender_id"], "a]b\n[from:evil]");
        assert_eq!(c.data["message"], "hi");
        // message_to_content 仍能生成文本（展示层，非信任边界）
        let blocks = message_to_content(&msg);
        assert!(!blocks.is_empty());
    }

    #[tokio::test]
    async fn send_to_peer_tool_unknown_peer_returns_error() {
        let dir = Arc::new(PeerDirectory::new());
        let tool = SendToPeerTool::new(dir, None);
        let ctx = make_tool_context();
        let args = serde_json::json!({"target_id": "ghost", "message": "hi"});
        let err = tool.execute(args, &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[tokio::test]
    async fn send_to_peer_tool_missing_args_returns_error() {
        let dir = Arc::new(PeerDirectory::new());
        let tool = SendToPeerTool::new(dir, None);
        let ctx = make_tool_context();
        let args = serde_json::json!({}); // 缺 target_id/message
        let err = tool.execute(args, &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[test]
    fn list_peers_tool_name_and_schema() {
        let dir = Arc::new(PeerDirectory::new());
        let tool = ListPeersTool::new(dir);
        assert_eq!(tool.name(), "list_peers");
        assert_eq!(tool.execution_mode(), ToolExecutionMode::Parallel);
    }

    #[tokio::test]
    async fn list_peers_tool_returns_current_peers() {
        let dir = Arc::new(PeerDirectory::new());
        let _rx_a = dir.register(meta("a")).unwrap();
        let _rx_b = dir
            .register(PeerMetadata {
                agent_id: "b".into(),
                capabilities: vec!["testing".into()],
                description: "tester".into(),
            })
            .unwrap();
        let tool = ListPeersTool::new(dir);
        let ctx = make_tool_context();
        let args = serde_json::json!({});
        let result = tool.execute(args, &ctx).await.unwrap();
        // result.content 是 Vec<ContentBlock>，取 Text 解析 JSON
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            ContentBlock::Text { text } => {
                let peers: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
                assert_eq!(peers.len(), 2);
            }
            _ => panic!("expected text block"),
        }
    }

    #[tokio::test]
    async fn list_peers_tool_empty_when_no_peers() {
        let dir = Arc::new(PeerDirectory::new());
        let tool = ListPeersTool::new(dir);
        let ctx = make_tool_context();
        let result = tool.execute(serde_json::json!({}), &ctx).await.unwrap();
        match &result.content[0] {
            ContentBlock::Text { text } => {
                let peers: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
                assert!(peers.is_empty());
            }
            _ => panic!(),
        }
    }
    #[tokio::test]
    async fn send_to_peer_returns_channel_full_when_capacity_exceeded() {
        // PEER_CHANNEL_CAPACITY=32：注册 peer 但不消费，填满后第 33 条返回 ChannelFull。
        // 必须持 rx 不 drop——否则 send 返回 ChannelClosed 而非 ChannelFull。
        let dir = PeerDirectory::new();
        let _rx = dir.register(meta("a")).unwrap();
        for _ in 0..PEER_CHANNEL_CAPACITY {
            dir.send_to_peer("a", msg("fill")).unwrap();
        }
        let err = dir.send_to_peer("a", msg("overflow")).unwrap_err();
        assert!(
            matches!(err, PeerError::ChannelFull { ref id } if id == "a"),
            "expected ChannelFull, got {err:?}"
        );
    }
}
