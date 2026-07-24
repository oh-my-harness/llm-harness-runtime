# MCP Adapter Content Handling Fix

**Date**: 2026-07-23  
**Issue**: [#78](https://github.com/oh-my-harness/llm-harness-runtime/issues/78) §5  
**前置 spec**: [MCP Transport, Protocol & Manager](./2026-07-23-mcp-transport-protocol-manager-design.md)  
**Scope**: `crates/llm-harness-runtime-mcp/src/lib.rs` 中 `McpToolAdapter::execute()` 的内容处理

## 背景

旧 `McpToolAdapter::execute()` 用 `filter_map(|c| c.text.map(|t| ContentBlock::Text { text: t }))` 只保留 text 内容块，丢弃 image/resource/blob。对返回图片截图、resource 引用的 MCP server（如 browser MCP、filesystem MCP 的图片预览），内容会静默丢失。

在主 spec 的三层重构后，旧 `McpContent`（flat struct，`text: Option<String>`）
被基于 MCP `2025-11-25` 的新 `McpContent` enum 取代，覆盖
`Text`/`Image`/`Audio`/`ResourceLink`/`Resource`。旧 `McpError` 被删除。
`McpSession::call_tool()` 返回 `Result<McpToolCallResult>`，工具执行错误通过
`McpToolCallResult.is_error` 标识，不再通过
`Err(McpError::ToolExecution)` 分支。

## 设计

参考 oh-my-pi 的 `formatMCPContent()`：保留所有 content block，非 text 降级为元信息描述。LLM 不能直接消费 image base64（除非多模态模型），但至少知道"这里有一张图片"比静默丢弃好。

### 转换规则

| McpContent 变体 | ContentBlock 输出 |
|----------------|-------------------|
| `Text { text }` | `ContentBlock::Text { text }` |
| `Image { data, mime_type }` | `ContentBlock::Text { text: "[Image: {mime_type}]" }` |
| `Audio { data, mime_type }` | `ContentBlock::Text { text: "[Audio: {mime_type}]" }` |
| `ResourceLink { uri, name, .. }` | `ContentBlock::Text { text: "[Resource link: {name} ({uri})]" }` |
| `Resource { resource: { uri, text, blob } }` | 如果有 `text`：`ContentBlock::Text { text: "[Resource: {uri}]\n{text}" }`；否则：`ContentBlock::Text { text: "[Resource: {uri}]" }` |

`McpToolCallResult.structured_content` 不拼接进 LLM 文本，原样保存到
`ToolResult.details`，供 UI、审计和后续结构化消费。

### 实现代码

```rust
fn format_mcp_content(content: Vec<McpContent>) -> Vec<ContentBlock> {
    content
        .into_iter()
        .map(|c| match c {
            McpContent::Text { text } => ContentBlock::Text { text },
            McpContent::Image { mime_type, .. } => {
                ContentBlock::Text {
                    text: format!("[Image: {mime_type}]"),
                }
            }
            McpContent::Audio { mime_type, .. } => ContentBlock::Text {
                text: format!("[Audio: {mime_type}]"),
            },
            McpContent::ResourceLink { uri, name, .. } => ContentBlock::Text {
                text: format!("[Resource link: {name} ({uri})]"),
            },
            McpContent::Resource { resource } => {
                let text = match resource.text {
                    Some(t) => format!("[Resource: {}]\n{}", resource.uri, t),
                    None => format!("[Resource: {}]", resource.uri),
                };
                ContentBlock::Text { text }
            }
        })
        .collect()
}
```

### execute() 完整实现

```rust
impl Tool for McpToolAdapter {
    fn name(&self) -> &str { &self.full_name }
    fn description(&self) -> &str { &self.description }
    fn parameters_schema(&self) -> &Value { &self.parameters_schema }

    fn execute<'a>(
        &'a self,
        args: Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, std::result::Result<ToolResult, ToolError>> {
        Box::pin(async move {
            let result = self.session
                .call_tool(&self.tool_name, args)
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;

            let details = result.structured_content.unwrap_or(Value::Null);
            let content = format_mcp_content(result.content);

            // 如果 MCP server 报告工具执行错误，在内容前加 "Error: " 前缀
            let content = if result.is_error {
                content.into_iter().map(|c| {
                    match c {
                        ContentBlock::Text { text } => ContentBlock::Text {
                            text: format!("Error: {text}"),
                        },
                        other => other,
                    }
                }).collect()
            } else {
                content
            };

            Ok(ToolResult {
                content,
                details,
                terminate: false,
            })
        })
    }
}
```

### 与旧实现的关键差异

| 旧实现 | 新实现 |
|--------|--------|
| `filter_map(c.text.map(...))` 丢弃非 text | `match` 保留所有变体，非 text 降级为描述 |
| `McpError::ToolExecution` 单独分支 | `McpToolCallResult.is_error` 布尔标志，统一路径 |
| `McpError::Protocol` → `ToolError::Execution` | `session.call_tool()` 的 `Err` → `ToolError::Execution` |
| 旧 `McpContent` flat struct（`text: Option<String>`） | 新 `McpContent` enum（Text/Image/Audio/ResourceLink/Resource） |
| 丢弃结构化工具结果 | `structuredContent` 保存到 `ToolResult.details` |

### 不做

- **不做多模态 image 传递**：不把 image base64 数据传给 LLM。runtime 的
  `ContentBlock` 目前没有 image/audio 媒体变体，多模态支持是另一个话题。
- **不做多模态 audio 传递**：只保留 MIME 元信息。
- **不做 blob 解码**：resource 的 `blob` 字段（base64 二进制）不解码，只显示 URI。

## 测试

| 测试 | 输入 | 期望 |
|------|------|------|
| 纯 text 内容 | `McpToolCallResult { content: vec![Text { text: "hello" }], is_error: false }` | `vec![ContentBlock::Text { text: "hello" }]` |
| 纯 image 内容 | `content: vec![Image { data: "...", mime_type: "image/png" }]` | `vec![ContentBlock::Text { text: "[Image: image/png]" }]` |
| 纯 audio 内容 | `content: vec![Audio { data: "...", mime_type: "audio/wav" }]` | `vec![ContentBlock::Text { text: "[Audio: audio/wav]" }]` |
| resource link | `content: vec![ResourceLink { uri: "file:///x", name: "x" }]` | 包含 name + URI 的描述 |
| resource 有 text | `content: vec![Resource { resource: { uri: "file:///x", text: Some("content"), blob: None } }]` | `vec![ContentBlock::Text { text: "[Resource: file:///x]\ncontent" }]` |
| resource 无 text | `content: vec![Resource { resource: { uri: "file:///x", text: None, blob: Some("...") } }]` | `vec![ContentBlock::Text { text: "[Resource: file:///x]" }]` |
| 混合内容 | `content: vec![text, image, resource]` | 三个 ContentBlock，顺序保持 |
| is_error=true | `content: vec![Text { text: "failed" }], is_error: true` | `vec![ContentBlock::Text { text: "Error: failed" }]` |
| session.call_tool() 网络错误 | `call_tool()` 返回 `Err` | `ToolError::Execution(...)` |
| structured content | `structured_content: Some(json!({"value": 1}))` | `ToolResult.details` 原样保留 |
