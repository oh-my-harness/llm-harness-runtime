# 内置工具指南

> `llm-harness-tools` 提供 9 个开箱即用的工具：`bash`、`read`、`write`、`edit`、`grep`、`glob`、`code_exec`、`web_search`、`web_fetch`。所有工具基于 `ExecutionEnv` 原语实现，不依赖具体操作系统。

---

## 注册方式

### 一键注册（推荐）

```rust
use llm_harness_tools::FsToolsPlugin;

// 注册 bash/read/write/edit/grep/glob，read/edit/write 共享 FileSnapshotStore
builder.install(&FsToolsPlugin::new(None));

// 或传入共享 snapshot store 启用 hashline 锚定
use llm_harness_tools::FileSnapshotStore;
use parking_lot::RwLock;
use std::sync::Arc;

let store = Arc::new(RwLock::new(FileSnapshotStore::new()));
builder.install(&FsToolsPlugin::new(Some(store)));
```

### 单独注册

```rust
use llm_harness_tools::{BashTool, ReadTool, WriteTool, EditTool, GrepTool, GlobTool, CodeExecTool, WebSearchTool, WebFetchTool};

builder
    .tool(BashTool::new())
    .tool(ReadTool::new(None))
    .tool(WriteTool::new(None))
    .tool(EditTool::new(None))
    .tool(GrepTool::new())
    .tool(GlobTool::new())
    .tool(CodeExecTool::new())
    .tool(WebSearchTool::new())
    .tool(WebFetchTool::new());
```

### Web 工具一键注册

```rust
use llm_harness_tools::WebToolsPlugin;

// 注册 web_search + web_fetch，共享 WebSearchConfig（默认 DuckDuckGo，无需 API key）
builder.install(&WebToolsPlugin::default());

// 自定义搜索 provider 与 API key
use llm_harness_tools::WebSearchConfig;
let config = WebSearchConfig {
    provider: "brave".into(),
    api_keys: [("brave".to_string(), "YOUR_KEY".to_string())].into_iter().collect(),
    ..Default::default()
};
builder.install(&WebToolsPlugin::new(config));
```

---

## code_exec

在临时目录中执行用户代码片段，支持 `python`、`bash`、`javascript` 三种语言。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `language` | string | ✅ | — | `python` / `bash` / `javascript` |
| `code` | string | ✅ | — | 要执行的代码 |

**行为**：
- 每次执行创建独立临时目录（`env.create_temp_dir("code_exec")`），写入源文件后执行
- 通过 `env.execute_shell` 执行——遵守沙箱隔离（bwrap/seatbelt）、abort 信号与超时（默认 30s，`CodeExecTool::with_timeout` 可调）
- 返回 `exit_code`、`stdout`、`stderr`；非零退出码**不是** ToolFailure，需检查输出中的 `exit_code`
- 结果为 projected：session 中保留摘要，完整输出按截断策略处理

**示例**：
```json
{"language": "python", "code": "print(sum(range(100)))"}
{"language": "javascript", "code": "console.log(Array.from({length: 5}, (_, i) => i * 2))"}
```

---

## web_search

搜索网页，返回带标题、URL、摘要的排序结果。支持 9 个 provider：

| Provider | 需要 API key | 说明 |
|----------|-------------|------|
| `duckduckgo`（默认） | ❌ | HTML 抓取 + 解析，开箱即用 |
| `bing` | ✅ | HTML 抓取解析 |
| `brave` | ✅ | JSON API |
| `tavily` | ✅ | JSON API，带相关度评分 |
| `serper` | ✅ | Google SERP API |
| `serpapi` | ✅ | Google SERP API |
| `exa` | ✅ | 神经搜索，带评分 |
| `searxng` | ❌ | 自建 SearXNG 实例（`base_url` 指向实例） |
| `google_cse` | ✅ | 需同时配置 `engine_id` |

`WebSearchConfig` 关键字段：`provider`、`base_url`、`api_key`（legacy 单 key）、`api_keys`（按 provider 的 key map，优先）、`engine_id`（仅 google_cse）、`max_results`（默认 5）、`fetch_timeout_secs`（默认 12）、`max_fetch_chars`（默认 12000）。

**行为**：
- 结果格式：`N. title\n   URL: ...\n   snippet`
- 结果为 projected：`details` 携带结构化结果（含 `sources` 引用元数据）
- provider 无结果/被拦截/HTML 变更时返回 `[WEB] No parseable ... results found` 提示

**示例**：
```json
{"query": "rust async runtime comparison tokio vs async-std"}
```

---

## web_fetch

抓取网页并提取可读文本（HTML 剥离）。

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `url` | string | ✅ | HTTP/HTTPS URL |

**行为**：
- 提取页面标题 + 正文文本（截断到 `max_fetch_chars`），返回元数据（`content_type`、`truncated`、`chars`、`sources` 引用块）
- 与 `web_search` 共享 `WebSearchConfig`（超时/字符上限）
- 不适用于二进制文件、图片、非 HTML 内容

**示例**：
```json
{"url": "https://doc.rust-lang.org/book/ch16-00-concurrency.html"}
```

> `code_exec`、`web_search`、`web_fetch` 与 `bash` 一样启用 `report_duration`，结果附 `[duration: ...]` 标注（见截断策略一节）。

---

## bash

执行 shell 命令，返回合并的 stdout + stderr。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `command` | string | ✅ | — | 要执行的命令 |
| `cwd` | string | ❌ | env 工作目录 | 工作目录（相对或绝对） |
| `timeout` | integer | ❌ | 300 | 超时秒数（1..3600） |
| `env` | object | ❌ | `{}` | 额外环境变量 |

**行为**：
- stdout 和 stderr 合并输出（stdout 在前）
- 退出码 0 → 正常结果；非 0 → 结果末尾附 `Command exited with code <n>`，`details.exit_code` 字段记录退出码（不是 ToolFailure）
- 输出超过 200 行或 10,000 字节 → 截断（保留首 50 行 + 尾 50 行），完整输出持久化到临时文件

**示例**：
```json
{"command": "git status"}
{"command": "cargo test", "timeout": 600}
{"command": "RUST_LOG=debug cargo run", "env": {"RUST_LOG": "debug"}}
```

---

## read

读取文件内容（带行号），或列出目录条目。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `path` | string | ✅ | — | 文件或目录路径 |
| `offset` | integer | ❌ | 1 | 起始行号（1-indexed） |
| `limit` | integer | ❌ | 300 | 返回行数上限 |
| `raw` | boolean | ❌ | false | 原始模式：无行号、无 header |

**行为**：
- **文件**：返回带行号的内容，格式 `[PATH#TAG]` header + `N:line`
- **hashline 锚定**：当 `FileSnapshotStore` 已附加且非 `raw` 模式时，文件内容被记录，输出带 `[PATH#TAG]` header（4-hex content hash），供后续 `edit` 检测过时内容
- **目录**：返回条目列表（最多 12 个），目录名带 `/` 后缀
- **大文件**：单次最多读 4 MiB；超出则只返回请求窗口 + 上下文行（前 1 行 + 后 3 行）

**示例**：
```json
{"path": "src/main.rs"}
{"path": "src/main.rs", "offset": 50, "limit": 100}
{"path": "config.json", "raw": true}
```

---

## write

覆盖写入文件，自动创建父目录。

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `path` | string | ✅ | 文件路径（相对或绝对） |
| `content` | string | ✅ | 完整文件内容 |

**行为**：
- 创建缺失的父目录
- 写入后使 `FileSnapshotStore` 中该路径的快照失效，防止后续 `edit` 对旧内容静默操作

**示例**：
```json
{"path": "src/main.rs", "content": "fn main() { println!(\"hi\"); }\n"}
```

---

## edit

对文件应用行级 swap / insert / delete 操作，带 hashline 过时检测。

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `path` | string | ✅ | 文件路径 |
| `operations` | array | ✅ | 操作列表，按序应用 |
| `tag` | string | ❌ | hashline 快照标签（4-hex），不匹配则拒绝 |

**操作类型**：

| 类型 | 字段 | 说明 |
|------|------|------|
| `swap` | `start`, `end`, `lines` | 替换 `start..=end` 行（1-indexed，inclusive） |
| `insert` | `start`, `lines` | 在 `start` 行前插入（`start == len+1` 追加到末尾） |
| `delete` | `start`, `end` | 删除 `start..=end` 行 |

**行为**：
- 行号 1-indexed，基于前一个操作后的状态
- `tag` + snapshot store → 验证标签匹配；不匹配 = 过时内容，拒绝操作
- 成功后刷新快照，返回新 tag 供后续 edit 锚定
- 文件总是重写为 joined lines + 单个 trailing `\n`（POSIX 约定）

**示例**：
```json
{"path": "src/main.rs", "operations": [
  {"type": "swap", "start": 5, "end": 8, "lines": ["    println!(\"hello\");"]}
]}
{"path": "src/main.rs", "tag": "A1B2", "operations": [
  {"type": "delete", "start": 20, "end": 25}
]}
```

---

## grep

按正则搜索文件内容，跨文件和目录。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `pattern` | string | ✅ | — | Rust regex（RE2 风格，无 lookaround/backreferences） |
| `paths` | array | ❌ | `["."]` | 搜索路径（文件或目录，目录递归） |
| `include` | string | ❌ | — | 文件名 glob 过滤（如 `*.rs`） |
| `case_sensitive` | boolean | ❌ | true | 是否区分大小写 |
| `context_before` | integer | ❌ | 0 | 匹配行前的上下文行数 |
| `context_after` | integer | ❌ | 0 | 匹配行后的上下文行数 |
| `max_results` | integer | ❌ | 200 | 最大匹配数 |

**行为**：
- 输出格式：`file_path:line_number:content`，上下文行前缀为空格
- 单行最长 512 字符（超出截断）
- 单文件最大 4 MiB，最多搜索 1000 个文件
- 上下文行上限 1000（防止大文件多匹配时内存爆炸）
- 结果超过阈值时截断 + 持久化（同 bash 的截断策略）

**示例**：
```json
{"pattern": "fn main\\(", "paths": ["src/main.rs"]}
{"pattern": "TODO|FIXME", "paths": ["src/"], "include": "*.rs"}
{"pattern": "password", "paths": ["config/"], "case_sensitive": false, "max_results": 10}
```

---

## glob

按 glob 模式查找文件。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| `paths` | array | ✅ | — | glob 模式或目录路径 |
| `max_results` | integer | ❌ | 500 | 最大返回数 |

**行为**：
- 裸模式（如 `*.rs`）自动加 `**/` 前缀
- 目录路径（无 glob 字符）自动搜索 `**/*`
- 结果去重 + 按修改时间排序（最新在前）
- 超过 `max_results` 时截断 + 持久化

**示例**：
```json
{"paths": ["**/*.rs"]}
{"paths": ["src/**/*.ts", "tests/**/*.ts"]}
{"paths": ["config/*.json"]}
```

---

## 截断策略

`bash`、`grep`、`glob` 共用截断逻辑（`TruncationConfig`）：

| 配置 | 默认值 | 说明 |
|------|--------|------|
| `max_lines` | 200 | 超过此行数触发截断 |
| `max_bytes` | 10,000 | 超过此字节数触发截断 |
| `head_lines` | 50 | 保留头部行数 |
| `tail_lines` | 50 | 保留尾部行数 |

截断时：完整输出持久化到临时文件 → 返回 head + `... [truncated N bytes, full output at /tmp/...]` + tail → LLM 可通过 `read` 工具读取完整输出。

**执行耗时标注**：`bash`、`code_exec`、`web_search`、`web_fetch` 的结果会附加 `[duration: ...]`（如 `[duration: 2.35s]`，1s 以内显示毫秒），由 agent loop 的工具装饰器按工具 opt-in 附加，成功与失败结果均携带。

---

## 安全注意事项

- **bash / code_exec**：应配合 `SafetyDefaultsPlugin`（命令黑名单）和 `Sandbox`（沙箱隔离）使用。无沙箱时 Agent 可执行任意命令；`code_exec` 通过 `env.execute_shell` 执行，继承沙箱配置。
- **write/edit**：应配合 `SafetyDefaultsPlugin`（路径遍历防护）。无防护时 Agent 可写入任意路径。
- **read/grep/glob**：路径解析基于 `env.working_dir()`，相对路径在工作目录下解析。
- **hashline 锚定**：`read` + `edit` 通过 `FileSnapshotStore` 共享 4-hex content tag，防止 LLM 对过时内容应用编辑。`write` 会使旧 tag 失效。
- **web_search/web_fetch**：抓取的外部内容可能包含提示注入。建议配合 `InjectionFilterPlugin`（注入短语过滤）与 `SourceTagPlugin`（`<external_content>` 来源包裹）使用。
