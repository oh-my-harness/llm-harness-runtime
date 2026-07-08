# 多 Provider Tool Message 转换调研与实现计划

## 背景

Issue #43 暴露了当前默认消息转换层的一个分层问题：

- OpenAI-compatible Chat API 要求 assistant 发出 `tool_calls` 后，所有对应 `tool` messages 必须紧跟其后。
- Anthropic Messages API 要求 tool result 作为 `role: "user"` 消息中的 `tool_result` content block 返回，并且消息角色需要满足 `user` / `assistant` 交替约束。
- 当前 `llm-harness-loop/src/convert.rs` 在 `DefaultConvertToLlm` 中无条件执行 `ensure_alternation()`，把 `AdapterMessage::Tool` 当作 user-like 消息处理。

这导致一个为 Anthropic 设计的 provider-specific 修正进入了默认通用转换层，在 OpenAI-compatible provider 上破坏 tool-call adjacency。

## Provider 协议差异

### OpenAI-compatible

OpenAI-compatible chat schema 中，工具调用和工具结果通常是独立角色：

```text
assistant: tool_calls = [call_a, call_b]
tool: tool_call_id = call_a
tool: tool_call_id = call_b
assistant: ...
```

关键约束是：assistant message 中声明的每个 `tool_call_id` 都必须由紧随其后的 tool messages 回答，中间不能插入 assistant/user message。

### Anthropic

Anthropic Messages API 的工具结果不是顶层 `role: "tool"` message，而是放在 follow-up user message 的 content blocks 中：

```json
{
  "role": "user",
  "content": [
    {
      "type": "tool_result",
      "tool_use_id": "toolu_...",
      "content": "..."
    }
  ]
}
```

Anthropic 文档还要求 continuation user message 中的 tool result blocks 对应前一个 assistant turn 的 tool use blocks。

参考：

- Anthropic tool use overview: https://docs.anthropic.com/en/docs/build-with-claude/tool-use/overview
- Anthropic handling stop reasons: https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons

## 开源实现调研

### LangChain

LangChain 核心层提供标准化 tool calling 接口，例如 `AIMessage.tool_calls` 和 `ToolMessage`，目的是屏蔽 OpenAI、Anthropic、Gemini 等 provider 的 tool-call schema 差异。

Anthropic 的特殊格式在 `langchain_anthropic` provider 集成中处理。其 `_format_messages()` 会把 LangChain 的通用消息转换成 Anthropic Messages API wire format；`_merge_messages()` 负责合并连续 human/tool 消息，避免 Anthropic 因连续 `user` role 报错。

结论：LangChain 把 provider-neutral 语义模型放在 core，把 Anthropic 的 user-role tool result 和 alternation/merge 规则放在 Anthropic adapter 层。

参考：

- Tool Calling with LangChain: https://www.langchain.com/blog/tool-calling-with-langchain
- Anthropic adapter source: https://github.com/langchain-ai/langchain/blob/master/libs/partners/anthropic/langchain_anthropic/chat_models.py

### Vercel AI SDK

Vercel AI SDK 使用统一的 model/message/tool 抽象，同时每个 provider 是独立包，例如 `@ai-sdk/anthropic`、`@ai-sdk/openai`。

Anthropic provider 明确面向 Anthropic Messages API，并暴露 Anthropic-specific provider options，例如 `disableParallelToolUse`。历史 issue 中也出现过 “tool result 后接 user 导致 Anthropic 连续 user role 错误” 的问题，讨论方向集中在 Anthropic provider 的转换逻辑，而不是把 Anthropic 规则提升为全局 message rule。

结论：AI SDK 也是核心统一抽象 + provider package 负责 wire-format 约束。

参考：

- Anthropic provider docs: https://ai-sdk.dev/providers/ai-sdk-providers/anthropic
- Related issue: https://github.com/vercel/ai/issues/2047

### LiteLLM

LiteLLM 对外提供 OpenAI-compatible 统一接口，但内部按 provider 做转换和清洗。

LiteLLM 的 message sanitization 文档明确说明，不同 provider 对 tool calling message format 有不同要求，尤其是 Anthropic；并且当前 sanitization 虽然逻辑可泛化，但只应用在 Anthropic message transformation pipeline。

结论：LiteLLM 把“自动修复 message shape”作为 provider transformation pipeline 的一部分，而不是通用消息转换的默认行为。

参考：

- Message Sanitization for Tool Calling: https://docs.litellm.ai/docs/completion/message_sanitization

### LlamaIndex

LlamaIndex 也有通用 `ChatMessage` / role / block 抽象，不同 LLM integration 自行转换。

在 Bedrock Converse 等 Claude/Anthropic 相关接口中，tool/function result 会被转换成 provider 要求的 user-role content block，并在 provider util 中处理相同 role 消息合并。

结论：LlamaIndex 同样倾向于在 provider integration 层消化 provider wire format 差异。

参考：

- Bedrock Converse utils: https://github.com/run-llama/llama_index/blob/main/llama-index-integrations/llms/llama-index-llms-bedrock-converse/llama_index/llms/bedrock_converse/utils.py
- LLM-agnostic tool call discussion: https://github.com/run-llama/llama_index/issues/19898

## 对本项目的判断

当前项目已有清晰的语义层和 adapter 层：

- `AgentMessage::ToolResult`：runtime 语义层的工具结果。
- `AdapterMessage::Tool`：adapter 抽象层的工具结果，不应预先承诺最终 wire role。
- OpenAI-compatible adapter：应把 `AdapterMessage::Tool` 序列化成 `role: "tool"` + `tool_call_id`。
- Anthropic adapter：应把 `AdapterMessage::Tool` 序列化成 `role: "user"` + `tool_result` content block。

因此 `DefaultConvertToLlm` 不应该无条件应用 Anthropic 的 alternation 规则。它应该只负责从 harness 语义消息转换到 provider-neutral adapter message。

## 实现计划

### 1. 收窄默认 converter 职责

修改 `crates/llm-harness-loop/src/convert.rs`：

- `DefaultConvertToLlm::convert()` 不再默认调用 `ensure_alternation()`。
- 保留 `AgentMessage::ToolResult -> AdapterMessage::Tool` 的语义映射。
- 更新注释，明确默认 converter 是 provider-neutral conversion。

对应测试调整：

- 将现有 `two_tools_in_a_row_separated_by_assistant` 改为断言连续 tool results 保持连续。
- 增加回归测试：`assistant(tool_use a,b) -> tool(a) -> tool(b)` 转换后不得插入空 assistant。
- 当前 `tool_result_followed_by_user_inserts_empty_assistant` 不再属于默认 converter 测试，应迁移到 Anthropic-specific 测试。

### 2. 增加 Anthropic-specific normalization

在 Anthropic adapter 或 Anthropic 专用 converter 中实现 normalization：

- 将 `AdapterMessage::Tool` 映射成 Anthropic `role: "user"` message 中的 `tool_result` block。
- 合并连续 user-like 消息，而不是简单插入空 assistant，优先满足 Anthropic 对 tool_result continuation 的要求。
- 当确实存在 `ToolResult -> User text` 这种跨 turn 连续 user-role 场景时，再采用 provider 层可接受的处理策略。

建议优先策略：

- 如果连续的 user-like 消息都属于同一个 assistant tool-use continuation，则合并到同一个 Anthropic user message。
- 如果 tool_result 后紧跟普通 user text，避免把普通 user text 混进必须只含 tool_result blocks 的 continuation message；应拆成后续 turn，并由 Anthropic adapter 决定是否插入 assistant separator 或报出结构化错误。

### 3. Provider 层测试矩阵

补充 provider-specific conversion 测试：

OpenAI-compatible:

- 多个 tool results 连续出现时保持 adjacency。
- assistant tool_calls 与 tool results 的 id 数量和顺序不被空 assistant 打断。

Anthropic:

- 多个 tool results 被放入 user message 的多个 `tool_result` blocks。
- 连续 user messages 被合并或通过明确策略处理。
- tool_result continuation 中不混入普通 text，否则会违反 Anthropic server-tool / client-tool continuation 约束。

### 4. 短期兼容方案

如果暂时找不到 Anthropic adapter 的集中转换点，可以先做一个较小修复：

- 修改 `ensure_alternation()`，不要在 `Tool + Tool` 之间插入 assistant。
- 仍在 `Tool + User`、`User + Tool`、`User + User` 之间插入 assistant。

这个方案可以快速修复 #43 的 OpenAI adjacency 问题，但不是最终分层。它仍然把 Anthropic 规则留在默认 converter 中，后续遇到 provider 差异时还会继续扩大例外分支。

## 推荐结论

推荐采用“provider-neutral default converter + provider-specific normalization”的方案。

最终目标是：

- runtime 保存语义正确的 tool result。
- adapter 抽象层不泄漏某个 provider 的 role 约束。
- OpenAI-compatible provider 保留 tool-call adjacency。
- Anthropic provider 独立处理 `role: "user"` + `tool_result` blocks、role alternation、message merge、empty content 等 Anthropic wire-format 约束。

这与 LangChain、Vercel AI SDK、LiteLLM、LlamaIndex 的主流分层做法一致，也能避免 provider 规则互相污染。

## 最终设计落地状态

当前分支已经按最终设计改造 runtime：

- `llm-api-adapter` 已更新到包含 provider-specific normalization 的提交 `69a868f5c8aa33059900f132e9c93366d1e479c3`。
- `DefaultConvertToLlm` 不再调用 `ensure_alternation()`。
- runtime 默认 converter 只做 `AgentMessage -> AdapterMessage` 的 provider-neutral conversion。
- `Tool + Tool`、`Tool + User`、`User + User`、`BranchSummary + User` 等输入在 adapter message 层保持原始结构，不插入 empty assistant。
- 回归测试明确 assistant 同轮多个 tool calls 后的多个 tool results 不会被空 assistant 打断。

Anthropic user/assistant alternation、tool_result merge、OpenAI tool-call adjacency 等 provider wire-format 约束已经从 runtime 默认 converter 的职责中移出，改由 provider adapter 负责。

## 与 Adapter 的配合关系

当前 runtime 侧依赖 adapter 的 provider-specific normalization，而不是在 runtime 中做兼容补丁。

原因是当前依赖版本的 Anthropic converter 已经具备关键能力：

- `AdapterMessage::Tool` 会被转换成 Anthropic `role: "user"` message 中的 `tool_result` block。
- 如果前一个 wire message 已经是 `role: "user"`，Anthropic converter 会把新的 `tool_result` block append 到该 user message。
- 因此连续 `AdapterMessage::Tool` 在进入 Anthropic adapter 后，可以合并为一个 user message 中的多个 `tool_result` blocks。

runtime 侧原来的问题在于：它在 adapter 看到连续 `Tool + Tool` 之前，就已经插入了空 assistant，导致 Anthropic adapter 无法合并，OpenAI adapter 也会收到被破坏的 tool adjacency。更根本的问题是：runtime 在 provider-neutral conversion 阶段执行了 Anthropic-specific normalization。

当前最终设计下，runtime 保持：

```text
Assistant(tool_calls=[c1, c2])
Tool(c1)
Tool(c2)
```

保持为：

```text
Assistant(...)
Tool(c1)
Tool(c2)
```

而不是：

```text
Assistant(...)
Tool(c1)
Assistant([])
Tool(c2)
```

这样：

- OpenAI-compatible adapter 可以直接输出合法的连续 `role: "tool"` messages。
- Anthropic adapter 可以把连续 `Tool` 合并为同一个 `role: "user"` message 中的多个 `tool_result` blocks。

## Adapter 改造结果

为了达到最终良好设计，`llm-api-adapter` 必须承担 provider-specific normalization，而不是只作为后续可选加固。adapter issue 已记录对应工作： https://github.com/oh-my-harness/llm-api-adapter/issues/10

adapter 侧已在 `69a868f5c8aa33059900f132e9c93366d1e479c3` 落地以下方向：

- Anthropic adapter 负责 canonical `User/Tool`、`Tool/Tool` 的 provider wire message normalization。
- OpenAI-compatible behavior 有回归测试覆盖，并继续保持独立的 `role: "tool"` messages。
- canonical `Message` 文档说明 provider adapters own provider-specific wire normalization。

adapter 侧仍应长期维护以下测试矩阵：

1. OpenAI-compatible conversion 测试：
   - assistant message 包含多个 tool calls。
   - 后续多个 `AdapterMessage::Tool` 必须连续输出为多个 `role: "tool"` messages。
   - 中间不得插入 empty assistant。

2. Anthropic conversion 测试：
   - 连续 `AdapterMessage::Tool` 合并为一个 `role: "user"` message。
   - 该 user message content 中包含多个 `tool_result` blocks。
   - 每个 `tool_result.tool_use_id` 保持原始 invocation id。

3. Anthropic edge case 策略测试：
   - `Tool + User`
   - `User + Tool`
   - `User + User`
   - 普通 user text 是否允许与 tool_result block 混在同一个 continuation user message 中。

其中第 3 点需要明确产品/协议策略。Anthropic 对 tool result continuation 的结构约束比 OpenAI 更强，如果普通 user text 被混入 tool_result continuation，可能产生更隐蔽的 provider reject。因此长期最好由 Anthropic adapter 做显式 normalization 或返回结构化错误，而不是由 runtime 插空 assistant 猜测。

## 目标实现路线

目标不是逐步堆 runtime 补丁，而是直接把最终设计落地：runtime/default converter provider-neutral，provider adapter 负责 provider wire-format。

本轮按以下顺序完成同一个最终方案：

### 第一步：Adapter 先补齐 provider-specific normalization

- 已在 `llm-api-adapter` main 完成，runtime 依赖更新到 `69a868f5c8aa33059900f132e9c93366d1e479c3`。

### 第二步：Runtime 移除 provider-specific workaround

- 已完成：`DefaultConvertToLlm` 只做 `AgentMessage -> AdapterMessage`。
- 已完成：删除默认调用的 `ensure_alternation()`。
- 已完成：删除 runtime 默认 converter 中的 Anthropic-specific 注释和测试。
- 已完成：保留 provider-neutral 的结构测试，连续 `Tool`、连续 `User` 等输入在 adapter message 层不被改写。

### 第三步：端到端验证

- runtime 使用新的 adapter 依赖版本。
- OpenAI-compatible 多工具调用场景通过。
- Anthropic 多工具调用场景通过。
- Anthropic 连续 user-like edge cases 有明确测试覆盖。

最终目标仍然是：runtime 保持 provider-neutral，provider adapter 负责 provider wire-format 约束。

## 合并标准

合并标准是：

- `llm-api-adapter` 已经承担 OpenAI/Anthropic provider-specific message normalization。
- runtime 默认 converter 不再执行 Anthropic alternation workaround。
- runtime 和 adapter 都有对应回归测试。
- 文档说明 runtime/provider adapter 的职责边界。
