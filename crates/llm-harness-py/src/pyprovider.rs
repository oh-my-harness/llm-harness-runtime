//! LLM provider 工厂：从 Python 侧创建内置 provider 实例。
//!
//! v1 只支持内置 provider（OpenAI/Anthropic），不暴露 `LlmClient` trait。
//! Python 侧通过 `create_openai_provider()` / `create_anthropic_provider()`
//! 创建不透明的 `PyProvider`，传给 `HarnessBuilder.provider(pattern, provider)`。

use std::sync::Arc;

use llm_harness_loop::{AnthropicProvider, LlmClient, OpenAIProvider};
use pyo3::prelude::*;

/// 不透明的 LLM provider 包装。持有 `Arc<dyn LlmClient>`。
#[pyclass(name = "Provider")]
pub struct PyProvider {
    pub(crate) client: Arc<dyn LlmClient>,
}

/// 创建 OpenAI 兼容 provider。
///
/// `base_url` 为空时使用默认 `https://api.openai.com`；
/// `parse_reasoning_content` 解析 DeepSeek 风格 reasoning_content；
/// `tolerant_keepalive` 容忍流中 keepalive 消息（DeepSeek 兼容）。
#[pyfunction]
#[pyo3(signature = (api_key, base_url=None, parse_reasoning_content=true, tolerant_keepalive=true))]
pub fn create_openai_provider(
    py: Python<'_>,
    api_key: &str,
    base_url: Option<&str>,
    parse_reasoning_content: bool,
    tolerant_keepalive: bool,
) -> PyResult<Py<PyProvider>> {
    let mut builder = OpenAIProvider::builder(api_key)
        .parse_reasoning_content(parse_reasoning_content)
        .tolerant_keepalive(tolerant_keepalive);
    if let Some(url) = base_url
        && !url.is_empty()
    {
        builder = builder.base_url(url);
    }
    let client: Arc<dyn LlmClient> = Arc::new(builder.build());
    Py::new(py, PyProvider { client })
}

/// 创建 Anthropic provider。
///
/// `base_url` 为空时使用默认 `https://api.anthropic.com`。
#[pyfunction]
#[pyo3(signature = (api_key, base_url=None))]
pub fn create_anthropic_provider(
    py: Python<'_>,
    api_key: &str,
    base_url: Option<&str>,
) -> PyResult<Py<PyProvider>> {
    let mut builder = AnthropicProvider::builder(api_key);
    if let Some(url) = base_url
        && !url.is_empty()
    {
        builder = builder.base_url(url);
    }
    let client: Arc<dyn LlmClient> = Arc::new(builder.build());
    Py::new(py, PyProvider { client })
}
