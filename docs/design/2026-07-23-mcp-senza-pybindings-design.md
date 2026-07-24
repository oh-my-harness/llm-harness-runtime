# MCP Senza PyO3 Bindings

**Date**: 2026-07-23  
**Issue**: [#78](https://github.com/oh-my-harness/llm-harness-runtime/issues/78) §6  
**前置 spec**:  
- [MCP Transport, Protocol & Manager](./2026-07-23-mcp-transport-protocol-manager-design.md)  
- [MCP Builder/Engine Wiring](./2026-07-23-mcp-builder-wiring-design.md)  
**Scope**: `Senza/src/` 新增 MCP PyO3 绑定  
**仓库**: [github.com/oh-my-harness/Senza](https://github.com/oh-my-harness/Senza)

## 背景

Senza 是 runtime 的 Python 绑定层。它通过 PyO3 镜像 Rust 侧 API：`pybuilder.rs` 镜像 `HarnessBuilder`，`pytool.rs` 镜像 `Tool`，`pyworkflow.rs` 镜像 `WorkflowEngine`。

当前 Senza `src/` 搜索 `mcp` 零匹配，`Cargo.toml` 无 `runtime-mcp` 依赖。Python 用户完全无法使用 MCP 功能。

## 设计

### 1. Cargo.toml 依赖

```toml
# Senza/Cargo.toml
[dependencies]
# ... 现有依赖 ...
llm-harness-runtime-mcp = { path = "../llm-harness-runtime/crates/llm-harness-runtime-mcp" }
```

### 2. 新增文件：`src/pymcp.rs`

#### 2.1 McpServerConfig 绑定

Python 侧用 dict 构建配置，Rust 侧反序列化：

```rust
use llm_harness_runtime_mcp::config::McpServerConfig;
use pyo3::prelude::*;

/// Python 侧的 MCP server 配置。
/// 用户通过 dict 传入，转换到 Rust McpServerConfig。
#[pyclass(name = "McpServerConfig")]
pub struct PyMcpServerConfig {
    inner: McpServerConfig,
}

#[pymethods]
impl PyMcpServerConfig {
    /// stdio server:
    /// McpServerConfig.stdio(command="npx", args=["-y", "@modelcontextprotocol/server-filesystem", "/tmp"])
    #[staticmethod]
    #[pyo3(signature = (command, args=None, env=None, cwd=None, timeout=None))]
    fn stdio(
        command: String,
        args: Option<Vec<String>>,
        env: Option<HashMap<String, String>>,
        cwd: Option<String>,
        timeout: Option<u64>,
    ) -> Self {
        Self {
            inner: McpServerConfig::Stdio {
                command,
                args: args.unwrap_or_default(),
                env: env.unwrap_or_default(),
                cwd,
                timeout,
            },
        }
    }

    /// HTTP server:
    /// McpServerConfig.http(url="https://example.com/mcp", headers=None, timeout=None)
    #[staticmethod]
    #[pyo3(signature = (url, headers=None, timeout=None))]
    fn http(
        url: String,
        headers: Option<HashMap<String, String>>,
        timeout: Option<u64>,
    ) -> Self {
        Self {
            inner: McpServerConfig::HttpOrSse {
                url,
                transport_type: None,
                headers: headers.unwrap_or_default(),
                timeout,
            },
        }
    }

    /// Legacy SSE server:
    /// McpServerConfig.sse(url="https://example.com/sse", headers=None, timeout=None)
    #[staticmethod]
    #[pyo3(signature = (url, headers=None, timeout=None))]
    fn sse(
        url: String,
        headers: Option<HashMap<String, String>>,
        timeout: Option<u64>,
    ) -> Self {
        Self {
            inner: McpServerConfig::HttpOrSse {
                url,
                transport_type: Some("sse".into()),
                headers: headers.unwrap_or_default(),
                timeout,
            },
        }
    }
}
```

#### 2.2 McpManager 绑定

```rust
use llm_harness_runtime_mcp::manager::McpManager;

/// Python 侧的 MCP Manager。
/// 管理多个 MCP server 的连接生命周期。
#[pyclass(name = "McpManager")]
pub struct PyMcpManager {
    inner: Arc<McpManager>,
}

#[pymethods]
impl PyMcpManager {
    #[new]
    fn new() -> Self {
        Self {
            inner: Arc::new(McpManager::new()),
        }
    }

    /// 添加并连接一个 MCP server。
    fn add_server(
        &self,
        py: Python<'_>,
        name: String,
        config: &PyMcpServerConfig,
    ) -> PyResult<PyObject> {
        let manager = self.inner.clone();
        let config = config.inner.clone();
        // 返回 coroutine
        Ok(pyo3_asyncio::tokio::future_into_py(py, async move {
            manager.add_server(&name, config).await
                .map_err(|e| PyErr::from(e))?;
            Ok(())
        })?.into())
    }

    /// 从 mcp.json 文件加载并连接所有 server。
    fn load_config_file(&self, py: Python<'_>, path: String) -> PyResult<PyObject> {
        let manager = self.inner.clone();
        Ok(pyo3_asyncio::tokio::future_into_py(py, async move {
            let config = McpConfigFile::from_file(Path::new(&path)).await
                .map_err(|e| PyErr::from(e))?;
            let configs: HashMap<String, McpServerConfig> = config.mcp_servers;
            manager.discover_and_connect(configs).await
                .map_err(|e| PyErr::from(e))?;
            Ok(())
        })?.into())
    }

    /// 获取所有已连接 server 的工具名。
    fn list_tools(&self) -> Vec<String> {
        self.inner.get_tools()
            .iter()
            .map(|t| t.full_name.clone())
            .collect()
    }

    /// 获取 server 连接状态。
    fn get_status(&self, name: String) -> String {
        match self.inner.get_connection_status(&name) {
            ConnectionStatus::Connected => "connected",
            ConnectionStatus::Connecting => "connecting",
            ConnectionStatus::Disconnected => "disconnected",
        }.to_string()
    }

    /// 重连指定 server。
    fn reconnect(&self, py: Python<'_>, name: String) -> PyResult<PyObject> {
        let manager = self.inner.clone();
        Ok(pyo3_asyncio::tokio::future_into_py(py, async move {
            manager.reconnect(&name).await
                .map_err(|e| PyErr::from(e))?;
            Ok(())
        })?.into())
    }

    /// 断开指定 server。
    fn disconnect_server(&self, py: Python<'_>, name: String) -> PyResult<PyObject> {
        let manager = self.inner.clone();
        Ok(pyo3_asyncio::tokio::future_into_py(py, async move {
            manager.disconnect_server(&name).await
                .map_err(|e| PyErr::from(e))?;
            Ok(())
        })?.into())
    }

    /// 断开所有 server。
    fn disconnect_all(&self, py: Python<'_>) -> PyResult<PyObject> {
        let manager = self.inner.clone();
        Ok(pyo3_asyncio::tokio::future_into_py(py, async move {
            manager.disconnect_all().await
                .map_err(|e| PyErr::from(e))?;
            Ok(())
        })?.into())
    }
}
```

#### 2.3 PyHarnessBuilder 扩展

Rust 侧现在使用 `McpHarnessBuilder` 包装普通 `HarnessBuilder`，因此 Python
builder 内部状态也要区分 Base/MCP，不能再调用不存在的
`HarnessBuilder::mcp_server()`：

```rust
// Senza/src/pybuilder.rs

enum PyBuilderState {
    Base(HarnessBuilder),
    Mcp(McpHarnessBuilder),
}

#[pymethods]
impl PyHarnessBuilder {
    /// 添加一个 MCP server。
    /// builder.mcp_server("filesystem", McpServerConfig.stdio(command="npx", ...))
    fn mcp_server(
        mut slf: PyRefMut<'_, Self>,
        name: String,
        config: &PyMcpServerConfig,
    ) -> PyResult<PyRefMut<'_, Self>> {
        // Base 首次调用时通过 HarnessBuilderMcpExt::with_mcp() 提升为 Mcp；
        // 已经是 Mcp 时继续追加配置。
        slf.map_builder_state(|state| state.add_mcp_server(name, config.inner.clone()))?;
        Ok(slf)
    }

    /// 记录 mcp.json 路径；文件在 await build() 阶段异步读取。
    /// 因此本方法保持同步和可链式调用。
    fn mcp_config_file(
        mut slf: PyRefMut<'_, Self>,
        path: String,
    ) -> PyResult<PyRefMut<'_, Self>> {
        slf.map_builder_state(|state| state.add_mcp_config_file(path.into()))?;
        Ok(slf)
    }

    /// 传入已创建的 McpManager（高级 API）。
    fn with_mcp_manager(
        mut slf: PyRefMut<'_, Self>,
        manager: &PyMcpManager,
    ) -> PyResult<PyRefMut<'_, Self>> {
        slf.map_builder_state(|state| state.with_mcp_manager(manager.inner.clone()))?;
        Ok(slf)
    }
}
```

`PyHarnessBuilder.build()` 在 async block 中匹配状态：

- `Base` → 调现有 `HarnessBuilder::build().await`
- `Mcp` → 调 `McpHarnessBuilder::build().await`

Python harness 内部相应持有 `AgentHarness` 或 `McpAgentHarness`。公共 prompt、
event、abort 等 API 委托给内部 harness；MCP harness 额外暴露
`list_mcp_prompts()`、`prompt_from_mcp()` 和 `shutdown()`。

#### 2.4 模块注册

```rust
// Senza/src/lib.rs (或 pymodule 注册处)

#[pymodule]
fn senza(_py: Python, m: &PyModule) -> PyResult<()> {
    // ... 现有注册 ...

    // MCP
    m.add_class::<PyMcpServerConfig>()?;
    m.add_class::<PyMcpManager>()?;
    // PyHarnessBuilder 的 mcp_server / mcp_config_file / with_mcp_manager
    // 作为 #[pymethods] 自动注册

    Ok(())
}
```

### 3. Python API 使用示例

```python
from senza import HarnessBuilder, McpServerConfig, McpManager

# 方式 1：编程式——通过 builder 链式调用
builder = (
    HarnessBuilder("claude-3-5-sonnet")
    .mcp_server("filesystem", McpServerConfig.stdio(
        command="npx",
        args=["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
    ))
    .mcp_server("remote", McpServerConfig.http(
        url="https://example.com/mcp",
        headers={"Authorization": "Bearer token"}
    ))
    .system_prompt("You are a helpful assistant.")
)

harness = await builder.build()

# 方式 2：配置文件驱动（路径同步记录，文件在 build 时异步读取）
builder = (
    HarnessBuilder("claude-3-5-sonnet")
    .mcp_config_file(".omp/mcp.json")
)
harness = await builder.build()

# 方式 3：外部 manager（手动管理生命周期）
manager = McpManager()
await manager.load_config_file(".omp/mcp.json")

builder = (
    HarnessBuilder("claude-3-5-sonnet")
    .with_mcp_manager(manager)
)

harness = await builder.build()

# 查看 MCP server 状态
print(manager.get_status("filesystem"))  # "connected"

# 重连
await manager.reconnect("filesystem")

# 断开所有
await manager.disconnect_all()
```

### 4. 异步处理

Senza 已有 tokio runtime 基础设施（`pybuilder.rs` 的 `build()` 方法在 GIL 释放后用全局 tokio runtime 执行 async 代码）。MCP 的异步方法（`add_server`、`load_config_file`、`reconnect` 等）沿用相同模式：

- 连接、build、重连、prompt 调用等异步操作在 Python 侧返回 coroutine
- `mcp_server()` / `mcp_config_file()` 只记录配置，保持同步链式调用
- Rust 侧 `pyo3_asyncio::tokio::future_into_py` 把 async fn 包装成 Python awaitable
- 在 tokio runtime 上执行

### 5. 不做

- **不做 MCP server 的 Python 绑定**：Senza 只暴露 MCP client 侧（消费 server），不暴露 server 侧（提供 server）。
- **不做 MCP 协议层的 Python 暴露**：不暴露 `McpTransport`、`McpSession`、`JsonRpcMessage` 等 Rust 内部类型。Python 用户只通过 `McpManager` 和 `HarnessBuilder` 交互。
- **不做 config 驱动的 YAML 支持**：只支持 mcp.json 格式。

### 6. 测试

| 测试 | 覆盖 |
|------|------|
| `McpServerConfig.stdio()` | 配置创建、字段映射 |
| `McpServerConfig.http()` | 配置创建、headers/timeout |
| `McpServerConfig.sse()` | 配置创建 |
| `builder.mcp_server()` | Base → MCP 状态提升、链式调用、build 后工具注册 |
| `builder.mcp_config_file()` | 同步记录路径、await build 时加载 mcp.json |
| `builder.with_mcp_manager()` | 外部 manager 传入 |
| `McpManager.list_tools()` | 工具名列表、`mcp__` 前缀 |
| `McpManager.get_status()` | 连接状态查询 |
| `McpManager.reconnect()` | 重连 |
| `McpManager.disconnect_all()` | 断开 |
| 集成测试 | 启动一个 stdio MCP server，Python 侧连接并调用工具 |
