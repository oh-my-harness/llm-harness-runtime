//! 规则谓词 trait。

/// 规则谓词。判断一个 tool call 是否满足条件。
///
/// `Send + Sync`，可跨线程共享（在 hook 里被并发读）。
pub trait Predicate: Send + Sync {
    /// 输入：tool name + 参数（已解析为 Value）。
    fn test(&self, tool_name: &str, args: &serde_json::Value) -> bool;

    /// 人类可读的谓词描述，用于 deny 消息（让模型知道被拒的具体原因）。
    /// 内置 predicate 均有实现；自定义 predicate 可覆盖，默认实现不泄露内部细节。
    fn describe(&self) -> String {
        "custom predicate".to_string()
    }
}
