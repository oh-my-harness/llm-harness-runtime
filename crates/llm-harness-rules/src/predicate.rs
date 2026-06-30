//! 规则谓词 trait。

/// 规则谓词。判断一个 tool call 是否满足条件。
///
/// `Send + Sync`，可跨线程共享（在 hook 里被并发读）。
/// 不加 `describe()`/序列化（YAGNI，需要时再加）。
pub trait Predicate: Send + Sync {
    /// 输入：tool name + 参数（已解析为 Value）。
    fn test(&self, tool_name: &str, args: &serde_json::Value) -> bool;
}
