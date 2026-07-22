use std::{
    any::{Any, TypeId},
    collections::HashMap,
    fmt,
    str::FromStr,
    sync::{Arc, Mutex, MutexGuard},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AgentMessage, ContentBlock, UserMessage};

type SharedRunValue = Arc<dyn Any + Send + Sync>;

/// 一次 Harness run 的唯一标识，使用按时间排序的 UUIDv7。
#[derive(Clone, Copy, Eq, PartialEq, Hash, Debug, Ord, PartialOrd)]
pub struct RunId(pub Uuid);

impl RunId {
    /// 生成新的 run 标识。
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for RunId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(value)?))
    }
}

impl Serialize for RunId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for RunId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_str(&value).map_err(serde::de::Error::custom)
    }
}

/// 应用在启动 run 时提供的进程内 typed extension 集合。
///
/// 该类型不实现 `Serialize` 或 `Debug`，避免认证上下文等请求数据被通用日志或
/// Session 持久化意外输出。`RunContext` 创建后只暴露只读访问。
#[derive(Default)]
pub struct RunExtensions {
    values: HashMap<TypeId, SharedRunValue>,
}

impl RunExtensions {
    /// 创建空的 extension 集合。
    pub fn new() -> Self {
        Self::default()
    }

    /// 插入或替换一种类型的 extension。
    pub fn insert<T>(&mut self, value: T)
    where
        T: Send + Sync + 'static,
    {
        self.values.insert(TypeId::of::<T>(), Arc::new(value));
    }

    /// 读取一种类型的 extension。
    pub fn get<T>(&self) -> Option<&T>
    where
        T: Send + Sync + 'static,
    {
        self.values
            .get(&TypeId::of::<T>())
            .and_then(|value| value.as_ref().downcast_ref::<T>())
    }

    /// 克隆一种类型的共享 extension 所有权。
    pub fn get_arc<T>(&self) -> Option<Arc<T>>
    where
        T: Send + Sync + 'static,
    {
        self.values
            .get(&TypeId::of::<T>())
            .cloned()
            .and_then(|value| value.downcast::<T>().ok())
    }

    /// 返回已注册的 extension 类型数量。
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// 返回集合是否为空。
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// runtime 和 Plugin 在单次 run 内共享的并发安全 typed state。
///
/// 值本身必须实现 `Send + Sync`。该容器不实现 `Serialize` 或 `Debug`，并随
/// `RunContext` 一起释放。
#[derive(Clone, Default)]
pub struct RunLocalState {
    values: Arc<Mutex<HashMap<TypeId, SharedRunValue>>>,
}

impl RunLocalState {
    /// 创建空的运行期状态容器。
    pub fn new() -> Self {
        Self::default()
    }

    fn lock_values(&self) -> MutexGuard<'_, HashMap<TypeId, SharedRunValue>> {
        self.values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 插入或替换一种类型的状态，返回被替换的旧值。
    pub fn insert<T>(&self, value: T) -> Option<Arc<T>>
    where
        T: Send + Sync + 'static,
    {
        self.lock_values()
            .insert(TypeId::of::<T>(), Arc::new(value))
            .and_then(|value| value.downcast::<T>().ok())
    }

    /// 读取一种类型的共享状态。
    pub fn get<T>(&self) -> Option<Arc<T>>
    where
        T: Send + Sync + 'static,
    {
        self.lock_values()
            .get(&TypeId::of::<T>())
            .cloned()
            .and_then(|value| value.downcast::<T>().ok())
    }

    /// 原子读取或初始化一种类型的共享状态。
    pub fn get_or_insert_with<T>(&self, create: impl FnOnce() -> T) -> Arc<T>
    where
        T: Send + Sync + 'static,
    {
        let mut values = self.lock_values();
        let type_id = TypeId::of::<T>();
        if let Some(value) = values.get(&type_id) {
            return value
                .clone()
                .downcast::<T>()
                .expect("RunLocalState TypeId must match its stored value");
        }

        let value = Arc::new(create());
        values.insert(type_id, value.clone());
        value
    }

    /// 返回当前状态类型数量。
    pub fn len(&self) -> usize {
        self.lock_values().len()
    }

    /// 返回状态容器是否为空。
    pub fn is_empty(&self) -> bool {
        self.lock_values().is_empty()
    }
}

/// 启动一次 Harness run 的应用请求。
#[derive(Default)]
pub struct RunRequest {
    /// 应用为本次 run 提供的原始输入消息。
    pub initial_messages: Vec<AgentMessage>,
    /// 不发送给模型、不写入 Session 的 typed 请求上下文。
    pub extensions: RunExtensions,
}

impl RunRequest {
    /// 使用明确的初始消息创建请求。
    pub fn new(initial_messages: Vec<AgentMessage>) -> Self {
        Self {
            initial_messages,
            extensions: RunExtensions::new(),
        }
    }

    /// 使用单条文本用户消息创建请求。
    pub fn from_text(text: impl Into<String>) -> Self {
        Self::new(vec![AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: Utc::now(),
        })])
    }

    /// 为请求增加一种 typed extension。
    pub fn with_extension<T>(mut self, extension: T) -> Self
    where
        T: Send + Sync + 'static,
    {
        self.extensions.insert(extension);
        self
    }
}

/// 单次 run 的不可变身份和输入上下文。
///
/// `extensions` 只读，`local` 只保存本次 run 的进程内状态。该类型不实现
/// `Serialize` 或 `Debug`。
pub struct RunContext {
    /// 本次 run 的唯一标识。
    id: RunId,
    /// 本次 run 的创建时间。
    started_at: DateTime<Utc>,
    /// 应用请求中的原始输入，不包含 Hook 或 runtime 后续注入的内容。
    initial_messages: Arc<[AgentMessage]>,
    /// 应用提供的只读 typed 请求上下文。
    extensions: RunExtensions,
    /// runtime 和 Plugin 使用的单次 run typed state。
    local: RunLocalState,
}

impl RunContext {
    /// 从应用请求创建新的 run 上下文。
    pub fn new(request: RunRequest) -> Self {
        Self {
            id: RunId::new(),
            started_at: Utc::now(),
            initial_messages: request.initial_messages.into(),
            extensions: request.extensions,
            local: RunLocalState::new(),
        }
    }

    /// 返回本次 run 的唯一标识。
    pub fn id(&self) -> RunId {
        self.id
    }

    /// 返回本次 run 的创建时间。
    pub fn started_at(&self) -> &DateTime<Utc> {
        &self.started_at
    }

    /// 返回应用请求中的原始输入消息。
    pub fn initial_messages(&self) -> &[AgentMessage] {
        &self.initial_messages
    }

    /// 读取应用提供的一种 typed request extension。
    pub fn extension<T>(&self) -> Option<&T>
    where
        T: Send + Sync + 'static,
    {
        self.extensions.get::<T>()
    }

    /// 克隆应用提供的一种共享 typed request extension。
    pub fn extension_arc<T>(&self) -> Option<Arc<T>>
    where
        T: Send + Sync + 'static,
    {
        self.extensions.get_arc::<T>()
    }

    /// 返回本次 run 的进程内 typed state。
    pub fn local(&self) -> &RunLocalState {
        &self.local
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    struct AccessContext(&'static str);

    #[test]
    fn run_id_serde_roundtrip() {
        let id = RunId::new();
        let encoded = serde_json::to_string(&id).unwrap();
        let decoded: RunId = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, id);
    }

    #[test]
    fn extensions_are_keyed_by_concrete_type() {
        let mut extensions = RunExtensions::new();
        extensions.insert(AccessContext("tenant-a"));
        extensions.insert(7_u64);

        assert_eq!(
            extensions.get::<AccessContext>(),
            Some(&AccessContext("tenant-a"))
        );
        assert_eq!(extensions.get::<u64>(), Some(&7));
        assert!(extensions.get::<String>().is_none());
    }

    #[test]
    fn local_state_initializes_each_type_once() {
        let local = RunLocalState::new();
        let calls = AtomicUsize::new(0);

        let first = local.get_or_insert_with(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            String::from("state")
        });
        let second = local.get_or_insert_with(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            String::from("other")
        });

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn local_state_recovers_after_initializer_panics() {
        let local = RunLocalState::new();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            local.get_or_insert_with::<String>(|| panic!("initializer failed"));
        }));
        assert!(panic.is_err());

        let recovered = local.get_or_insert_with(|| String::from("recovered"));
        assert_eq!(recovered.as_str(), "recovered");
        assert_eq!(local.len(), 1);
    }

    #[test]
    fn run_context_freezes_request_input_and_extensions() {
        let context = RunContext::new(
            RunRequest::from_text("hello").with_extension(AccessContext("tenant-a")),
        );

        assert_eq!(context.initial_messages().len(), 1);
        assert_eq!(
            context.extension::<AccessContext>(),
            Some(&AccessContext("tenant-a"))
        );
        assert!(context.local().is_empty());
    }

    #[test]
    fn run_context_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RunContext>();
    }
}
