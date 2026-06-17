# AuthHook — 设计废线

发现时间：2026-06-17

## 现状

`AuthHook` trait 和 `AuthInfo` 定义在 `llm-harness-types/src/hooks.rs`，通过以下路径传递但从未消费：

```
hooks.rs 定义 AuthHook / AuthInfo
  ↓
config.rs LoopConfig.auth: Option<Arc<dyn AuthHook>>
  ↓
harness.rs HarnessOptions.auth → build_loop_config() → LoopConfig.auth
  ↓
agent.rs LoopConfig.auth = None
  ↓
loop_fn.rs LoopConfig.auth 被 build() 接受，未使用
```

具体证据：

| 文件 | 行 | 行为 |
|------|-----|------|
| `hooks.rs:187-203` | 定义 `AuthInfo` + `AuthHook` trait | — |
| `config.rs:133` | `LoopConfig.auth: Option<Arc<dyn AuthHook>>` | 存储 |
| `harness.rs:309,432,511,1236,1526` | 传参、clone、unwrap 到 `&dyn AuthHook` | 仅传参 |
| `loop_fn.rs:614` | `auth: None` | 默认空 |
| `agent.rs:414` | `auth: None` | 默认空 |

`grep -rn 'resolve\|\.auth' loop_fn.rs` 结果：`auth` 只作为配置字段存在，`resolve()` 从未被调用。

## 为什么变成废线

认证实际发生在 **adapter 层**——`OpenAIProvider::builder("sk-xxx")` 在构造 provider 时就拿到了 API key。Loop 层不需要也不应该介入认证。AuthHook 试图在 loop 层再做一次认证抽象，但 loop 层没有使用它的理由——它不创建 provider 实例，只管调用 `client.chat_stream()`。

## 建议

1. 删除 `AuthHook` trait 和 `AuthInfo` struct（hooks.rs）
2. 删除 `LoopConfig.auth` 字段（config.rs）
3. 删除 `HarnessOptions.auth` 及所有传参链（harness.rs, agent.rs）
4. 删除 `llm-harness-runtime-auth` crate（或合并成 example 代码）
5. 总删除量约 80 行，不涉及功能损失
