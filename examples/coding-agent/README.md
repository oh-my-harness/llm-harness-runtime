# coding-agent

基于 `llm-harness` 框架的 CLI 编程助手。

**Phase 1** 提供 4 个工具（read / bash / edit / write）和三种运行模式。

## 运行模式

### 1. 单次查询（one-shot）

参数形式：

```
cargo run -- "用 rust 写一个简短的 http server"
```

`-p` 显式标志：

```
cargo run -- -p "添加超时控制"
```

### 2. 管道输入（pipe）

```
echo "列出当前目录" | cargo run --
```

非 TTY 的 stdin 自动触发 pipe 模式。

### 3. 交互式 REPL

```
cargo run -- -i
```

逐轮输入对话，支持 `exit` / `quit` 退出，自动保存历史记录。

## 环境变量

| 变量 | 说明 | 必填 |
|------|------|------|
| `ANTHROPIC_API_KEY` | Anthropic API 密钥 | 二选一 |
| `ANTHROPIC_API_BASE` | Anthropic API 地址（默认 `https://api.anthropic.com`） | 可选 |
| `ANTHROPIC_MODEL` | Anthropic 模型名，默认 `claude-sonnet-4-6` | 可选 |
| `OPENAI_API_KEY` | OpenAI 兼容 API 密钥 | 二选一 |
| `OPENAI_API_BASE` | OpenAI 兼容 API 地址（如 `https://api.siliconflow.cn/v1`） | 使用 OpenAI 时必填 |
| `OPENAI_MODEL` | OpenAI 兼容模型名，默认 `gpt-4o` | 可选 |
| `CODING_AGENT_MODEL` | 覆盖所有 provider 的默认模型 | 可选 |

### 使用 Anthropic

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
cargo run -- -p "分析当前项目结构"
```

使用自定义 Anthropic 兼容网关：

```bash
export ANTHROPIC_API_KEY="sk-..."
export ANTHROPIC_API_BASE="https://your-gateway.com/anthropic"
export ANTHROPIC_MODEL="claude-sonnet-4-6"
cargo run -- -i
```

### 使用 OpenAI 兼容 API

```bash
export OPENAI_API_KEY="sk-..."
export OPENAI_API_BASE="https://api.siliconflow.cn/v1"
export OPENAI_MODEL="Qwen/Qwen2.5-Coder-14B-Instruct"
cargo run -- -i
```

## 构建

```bash
cargo build --release
./target/release/coding-agent -p "你好"
```

## 当前工具（Phase 1）

| 工具 | 功能 |
|------|------|
| `read` | 读取文件内容 |
| `bash` | 执行 shell 命令 |
| `edit` | 编辑现有文件 |
| `write` | 写入新文件 |
