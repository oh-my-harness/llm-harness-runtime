use futures::future::BoxFuture;
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;

use crate::text_decode::decode_response_text;

static SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Search query. e.g. \"rust async runtime tokio\" or \"openai gpt-4o release date\"" }
        },
        "required": ["query"]
    })
});

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebSearchConfig {
    pub provider: String,
    pub base_url: String,
    /// Legacy single API key — used as fallback when `api_keys` has no entry
    /// for the active provider.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Per-provider API keys. Keys are lowercased provider names
    #[serde(default)]
    pub api_keys: std::collections::HashMap<String, String>,
    /// Google CSE engine ID (cx parameter). Only used when provider is `google_cse`.
    #[serde(default)]
    pub engine_id: Option<String>,
    pub max_results: usize,
    pub fetch_timeout_secs: u64,
    pub max_fetch_chars: usize,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            provider: "duckduckgo".into(),
            base_url: "https://duckduckgo.com/html/".into(),
            api_key: None,
            api_keys: std::collections::HashMap::new(),
            engine_id: None,
            max_results: 5,
            fetch_timeout_secs: 12,
            max_fetch_chars: 12_000,
        }
    }
}

pub struct WebSearchTool {
    config: WebSearchConfig,
    client: reqwest::Client,
}

impl WebSearchTool {
    pub fn new() -> Self {
        Self::with_config(WebSearchConfig::default())
    }

    pub fn with_config(config: WebSearchConfig) -> Self {
        let timeout_secs = config.fetch_timeout_secs.clamp(3, 60);
        Self {
            config,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout_secs))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn report_duration(&self) -> bool {
        true
    }

    fn description(&self) -> &str {
        "Search the web and return ranked results with titles, URLs, and snippets.\n\n\
         Use when: answering questions about current events, recent releases, or externally verifiable facts; finding documentation, papers, or library APIs; looking up real-world entities, products, or people.\n\
         Don't use for: fetching full page content from a known URL (use web_fetch); executing code (use code_exec); answering from your own training data when the topic is stable and well-known.\n\
         Returns: Up to max_results results, each with title, URL, and snippet. Results are ranked by relevance.\n\n\
         Examples:\n\
         {\"query\": \"rust async runtime comparison tokio vs async-std\"}\n\
         {\"query\": \"openai gpt-4o pricing per token 2024\"}\n\
         {\"query\": \"how to parse HTML in python with beautifulsoup\"}"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        &SCHEMA
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let query = args["query"].as_str().unwrap_or("").to_string();
            if query.trim().is_empty() {
                return Err(ToolFailure::invalid_arguments("query is empty"));
            }

            let results = self
                .search(&query)
                .await
                .map_err(|err| ToolFailure::new("web_search_failed", err.to_string()))?;
            let text = format_results(&self.config.provider, &query, &results);
            let model_content = vec![DataBlock::text(text)];
            Ok(ToolResult::projected(
                model_content.clone(),
                model_content,
                json!({
                    "query": query,
                    "provider": self.config.provider,
                    "results": results.len(),
                    "items": result_details(&results),
                    "sources": source_details(&results),
                }),
                false,
            ))
        })
    }
}

impl WebSearchTool {
    async fn search(&self, query: &str) -> anyhow::Result<Vec<SearchResult>> {
        match self.config.provider.trim().to_ascii_lowercase().as_str() {
            "duckduckgo" => self.search_duckduckgo(query).await,
            "bing" => self.search_bing(query).await,
            "brave" => self.search_brave(query).await,
            "tavily" => self.search_tavily(query).await,
            "serper" => self.search_serper(query).await,
            "serpapi" => self.search_serpapi(query).await,
            "exa" => self.search_exa(query).await,
            "searxng" => self.search_searxng(query).await,
            "google_cse" => self.search_google_cse(query).await,
            other => anyhow::bail!("unsupported web search provider `{other}`"),
        }
    }

    async fn search_duckduckgo(&self, query: &str) -> anyhow::Result<Vec<SearchResult>> {
        match self.search_duckduckgo_lite(query).await {
            Ok(results) if !results.is_empty() => return Ok(results),
            Ok(_) => {
                tracing::debug!("duckduckgo lite returned no results, falling back to html");
            }
            Err(e) => {
                tracing::debug!("duckduckgo lite failed ({e}), falling back to html");
            }
        }

        self.search_duckduckgo_html(query).await
    }

    async fn search_duckduckgo_lite(&self, query: &str) -> anyhow::Result<Vec<SearchResult>> {
        let limit = self.config.max_results.clamp(1, 10);
        let region = duckduckgo_region(query);
        let response = self
            .client
            .post("https://lite.duckduckgo.com/lite/")
            .form(&[("q", query), ("kl", region)])
            .header(reqwest::header::USER_AGENT, browser_user_agent())
            .header(reqwest::header::ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.8")
            .send()
            .await?
            .error_for_status()?;
        let headers = response.headers().clone();
        let bytes = response.bytes().await?;
        let html = decode_response_text(&headers, &bytes);

        Ok(parse_duckduckgo_lite_results(&html, limit))
    }

    async fn search_duckduckgo_html(&self, query: &str) -> anyhow::Result<Vec<SearchResult>> {
        let limit = self.config.max_results.clamp(1, 10);
        let response = self
            .client
            .get(self.config.base_url.trim())
            .query(&[("q", query)])
            .header(reqwest::header::USER_AGENT, browser_user_agent())
            .header(reqwest::header::ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.8")
            .send()
            .await?
            .error_for_status()?;
        let headers = response.headers().clone();
        let bytes = response.bytes().await?;
        let html = decode_response_text(&headers, &bytes);
        Ok(parse_duckduckgo_html_results(&html, limit))
    }

    async fn search_bing(&self, query: &str) -> anyhow::Result<Vec<SearchResult>> {
        let limit = self.config.max_results.clamp(1, 10);
        let response = self
            .client
            .get(self.config.base_url.trim())
            .query(&[("q", query)])
            .header(reqwest::header::USER_AGENT, browser_user_agent())
            .header(reqwest::header::ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.8")
            .send()
            .await?
            .error_for_status()?;
        let headers = response.headers().clone();
        let bytes = response.bytes().await?;
        let html = decode_response_text(&headers, &bytes);
        Ok(parse_bing_html_results(&html, limit))
    }

    async fn search_brave(&self, query: &str) -> anyhow::Result<Vec<SearchResult>> {
        let limit = self.config.max_results.clamp(1, 10);
        let api_key = self.require_api_key("Brave")?;
        let response = self
            .client
            .get(self.config.base_url.trim())
            .query(&[("q", query.to_string()), ("count", limit.to_string())])
            .header("X-Subscription-Token", api_key)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await?
            .error_for_status()?;
        let value = response.json::<serde_json::Value>().await?;
        Ok(parse_brave_results(&value, limit))
    }

    async fn search_tavily(&self, query: &str) -> anyhow::Result<Vec<SearchResult>> {
        let limit = self.config.max_results.clamp(1, 10);
        let api_key = self.require_api_key("Tavily")?;
        let response = self
            .client
            .post(self.config.base_url.trim())
            .bearer_auth(api_key)
            .json(&json!({
                "query": query,
                "max_results": limit,
                "search_depth": "basic",
                "include_answer": false,
                "include_raw_content": false,
            }))
            .send()
            .await?
            .error_for_status()?;
        let value = response.json::<serde_json::Value>().await?;
        Ok(parse_tavily_results(&value, limit))
    }

    async fn search_serper(&self, query: &str) -> anyhow::Result<Vec<SearchResult>> {
        let limit = self.config.max_results.clamp(1, 10);
        let api_key = self.require_api_key("Serper")?;
        let response = self
            .client
            .post(self.config.base_url.trim())
            .header("X-API-KEY", api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&json!({
                "q": query,
                "num": limit,
            }))
            .send()
            .await?
            .error_for_status()?;
        let value = response.json::<serde_json::Value>().await?;
        Ok(parse_serper_results(&value, limit))
    }

    async fn search_serpapi(&self, query: &str) -> anyhow::Result<Vec<SearchResult>> {
        let limit = self.config.max_results.clamp(1, 10);
        let api_key = self.require_api_key("SerpAPI")?;
        let response = self
            .client
            .get(self.config.base_url.trim())
            .query(&[
                ("engine", "google".to_string()),
                ("q", query.to_string()),
                ("api_key", api_key.to_string()),
                ("num", limit.to_string()),
            ])
            .send()
            .await?
            .error_for_status()?;
        let value = response.json::<serde_json::Value>().await?;
        Ok(parse_serpapi_results(&value, limit))
    }

    async fn search_exa(&self, query: &str) -> anyhow::Result<Vec<SearchResult>> {
        let limit = self.config.max_results.clamp(1, 10);
        let api_key = self.require_api_key("Exa")?;
        let response = self
            .client
            .post(self.config.base_url.trim())
            .header("x-api-key", api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&json!({
                "query": query,
                "num_results": limit,
                "type": "auto",
                "contents": { "text": true },
            }))
            .send()
            .await?
            .error_for_status()?;
        let value = response.json::<serde_json::Value>().await?;
        Ok(parse_exa_results(&value, limit))
    }

    fn require_api_key(&self, provider: &str) -> anyhow::Result<&str> {
        let key_lower = provider.to_ascii_lowercase();
        // Check per-provider keys first, then fall back to legacy single key.
        if let Some(key) = self.config.api_keys.get(&key_lower) {
            let trimmed = key.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed);
            }
        }
        self.config
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .ok_or_else(|| anyhow::anyhow!("{provider} API key is not configured"))
    }

    async fn search_searxng(&self, query: &str) -> anyhow::Result<Vec<SearchResult>> {
        let limit = self.config.max_results.clamp(1, 10);
        let base_url = self.config.base_url.trim();
        let response = self
            .client
            .get(base_url)
            .query(&[
                ("q", query.to_string()),
                ("format", "json".to_string()),
                ("pageno", "1".to_string()),
            ])
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await?
            .error_for_status()?;
        let value = response.json::<serde_json::Value>().await?;
        Ok(parse_searxng_results(&value, limit))
    }

    async fn search_google_cse(&self, query: &str) -> anyhow::Result<Vec<SearchResult>> {
        let limit = self.config.max_results.clamp(1, 10);
        let api_key = self.require_api_key("google_cse")?;
        let base_url = if self.config.base_url.trim().is_empty()
            || self.config.base_url.trim() == "https://duckduckgo.com/html/"
        {
            "https://www.googleapis.com/customsearch/v1"
        } else {
            self.config.base_url.trim()
        };
        let response = self
            .client
            .get(base_url)
            .query(&[
                ("key", api_key.to_string()),
                ("cx", self.config.engine_id.clone().unwrap_or_default()),
                ("q", query.to_string()),
                ("num", limit.to_string()),
            ])
            .send()
            .await?
            .error_for_status()?;
        let value = response.json::<serde_json::Value>().await?;
        Ok(parse_google_cse_results(&value, limit))
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub score: Option<f32>,
    pub source: Option<String>,
}

impl SearchResult {
    fn new(
        title: impl Into<String>,
        url: impl Into<String>,
        snippet: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            title: title.into(),
            url: url.into(),
            snippet: snippet.into(),
            score: None,
            source: Some(source.into()),
        }
    }
}

fn result_with_score(
    title: impl Into<String>,
    url: impl Into<String>,
    snippet: impl Into<String>,
    source: impl Into<String>,
    score: Option<f32>,
) -> SearchResult {
    SearchResult {
        title: title.into(),
        url: url.into(),
        snippet: snippet.into(),
        score,
        source: Some(source.into()),
    }
}

fn duckduckgo_region(_query: &str) -> &'static str {
    // Default to worldwide (wt-wt). The previous heuristic routed any non-ASCII
    // query to cn-zh, which misrouted Cyrillic, Japanese, and accented Latin
    // queries. Callers can configure a region via DuckDuckGo's `kl` parameter
    // if needed.
    "wt-wt"
}

fn normalize_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn browser_user_agent() -> &'static str {
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36"
}

fn format_results(provider: &str, query: &str, results: &[SearchResult]) -> String {
    if results.is_empty() {
        return format!(
            "[WEB] No parseable {provider} results found for \"{query}\". The search provider may have returned no matches, blocked the request, or changed its HTML structure."
        );
    }

    let body = results
        .iter()
        .enumerate()
        .map(|(index, result)| {
            format!(
                "{}. {}\n   URL: {}\n   {}",
                index + 1,
                result.title,
                result.url,
                result.snippet
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("[WEB] {provider} results for \"{query}\":\n{body}")
}

fn result_details(results: &[SearchResult]) -> serde_json::Value {
    serde_json::Value::Array(
        results
            .iter()
            .map(|result| {
                json!({
                    "title": result.title,
                    "url": result.url,
                    "snippet": result.snippet,
                    "score": result.score,
                    "source": result.source,
                })
            })
            .collect(),
    )
}

fn source_details(results: &[SearchResult]) -> serde_json::Value {
    serde_json::Value::Array(
        results
            .iter()
            .enumerate()
            .map(|(index, result)| {
                json!({
                    "index": index + 1,
                    "kind": "web",
                    "source": result.title,
                    "title": result.title,
                    "url": result.url,
                    "text": result.snippet,
                    "score": result.score,
                })
            })
            .collect(),
    )
}

fn parse_brave_results(value: &serde_json::Value, limit: usize) -> Vec<SearchResult> {
    value
        .pointer("/web/results")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .take(limit)
        .filter_map(|item| {
            let title = json_text(item, "title");
            let url = json_text(item, "url");
            let snippet = json_text(item, "description");
            (!title.is_empty() && !url.is_empty())
                .then(|| SearchResult::new(title, url, snippet, "brave"))
        })
        .collect()
}

fn parse_tavily_results(value: &serde_json::Value, limit: usize) -> Vec<SearchResult> {
    value
        .get("results")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .take(limit)
        .filter_map(|item| {
            let title = json_text(item, "title");
            let url = json_text(item, "url");
            let snippet = json_text(item, "content");
            let score = json_score(item, "score");
            (!title.is_empty() && !url.is_empty())
                .then(|| result_with_score(title, url, snippet, "tavily", score))
        })
        .collect()
}

fn parse_serper_results(value: &serde_json::Value, limit: usize) -> Vec<SearchResult> {
    value
        .get("organic")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .take(limit)
        .filter_map(|item| {
            let title = json_text(item, "title");
            let url = json_text(item, "link");
            let snippet = json_text(item, "snippet");
            (!title.is_empty() && !url.is_empty())
                .then(|| SearchResult::new(title, url, snippet, "serper"))
        })
        .collect()
}

fn parse_serpapi_results(value: &serde_json::Value, limit: usize) -> Vec<SearchResult> {
    value
        .get("organic_results")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .take(limit)
        .filter_map(|item| {
            let title = json_text(item, "title");
            let url = json_text(item, "link");
            let snippet = json_text(item, "snippet");
            (!title.is_empty() && !url.is_empty())
                .then(|| SearchResult::new(title, url, snippet, "serpapi"))
        })
        .collect()
}

fn parse_exa_results(value: &serde_json::Value, limit: usize) -> Vec<SearchResult> {
    value
        .get("results")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .take(limit)
        .filter_map(|item| {
            let title = json_text(item, "title");
            let url = json_text(item, "url");
            let snippet = json_text(item, "text");
            let score = json_score(item, "score");
            (!title.is_empty() && !url.is_empty())
                .then(|| result_with_score(title, url, snippet, "exa", score))
        })
        .collect()
}

fn parse_searxng_results(value: &serde_json::Value, limit: usize) -> Vec<SearchResult> {
    value
        .get("results")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .take(limit)
        .filter_map(|item| {
            let title = json_text(item, "title");
            let url = json_text(item, "url");
            let snippet = json_text(item, "content");
            (!title.is_empty() && !url.is_empty())
                .then(|| SearchResult::new(title, url, snippet, "searxng"))
        })
        .collect()
}

fn parse_google_cse_results(value: &serde_json::Value, limit: usize) -> Vec<SearchResult> {
    value
        .get("items")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .take(limit)
        .filter_map(|item| {
            let title = json_text(item, "title");
            let url = json_text(item, "link");
            let snippet = json_text(item, "snippet");
            (!title.is_empty() && !url.is_empty())
                .then(|| SearchResult::new(title, url, snippet, "google_cse"))
        })
        .collect()
}

fn json_text(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(normalize_text)
        .unwrap_or_default()
}

fn json_score(value: &serde_json::Value, key: &str) -> Option<f32> {
    value
        .get(key)
        .and_then(serde_json::Value::as_f64)
        .filter(|score| score.is_finite())
        .map(|score| score as f32)
}

fn parse_duckduckgo_lite_results(html: &str, limit: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut rest = html;

    while results.len() < limit {
        let Some(link_marker_pos) = rest.find("result-link") else {
            break;
        };
        let anchor_pos = rest[..link_marker_pos]
            .rfind("<a")
            .unwrap_or(link_marker_pos);
        rest = &rest[anchor_pos..];

        let Some(href) = attr_value(rest, "href") else {
            rest = &rest["result-link".len()..];
            continue;
        };
        let Some(anchor_end) = rest.find("</a>") else {
            break;
        };
        let title_start = rest.find('>').map(|index| index + 1).unwrap_or(0);
        let title = normalize_text(&strip_html_fragment(&rest[title_start..anchor_end]));
        let url = normalize_duckduckgo_url(&decode_entities(&href));

        let next_anchor = rest[anchor_end..]
            .find("result-link")
            .map(|index| anchor_end + index)
            .unwrap_or(rest.len());
        let block = &rest[..next_anchor];
        let snippet = extract_lite_snippet(block).unwrap_or_default();

        if !title.is_empty() && !url.is_empty() {
            results.push(SearchResult::new(title, url, snippet, "duckduckgo-lite"));
        }

        rest = &rest[next_anchor..];
    }

    results
}

fn parse_duckduckgo_html_results(html: &str, limit: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut rest = html;

    while results.len() < limit {
        let Some(anchor_pos) = rest.find("result__a") else {
            break;
        };
        rest = &rest[anchor_pos..];

        let Some(href) = attr_value(rest, "href") else {
            rest = &rest["result__a".len()..];
            continue;
        };
        let Some(anchor_end) = rest.find("</a>") else {
            break;
        };
        let title_start = rest.find('>').map(|index| index + 1).unwrap_or(0);
        let title = normalize_text(&strip_html_fragment(&rest[title_start..anchor_end]));
        let url = normalize_duckduckgo_url(&decode_entities(&href));

        let next_anchor = rest[anchor_end..]
            .find("result__a")
            .map(|index| anchor_end + index)
            .unwrap_or(rest.len());
        let block = &rest[..next_anchor];
        let snippet = extract_html_snippet(block).unwrap_or_default();

        if !title.is_empty() && !url.is_empty() {
            results.push(SearchResult::new(title, url, snippet, "duckduckgo"));
        }

        rest = &rest[next_anchor..];
    }

    results
}

fn parse_bing_html_results(html: &str, limit: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut rest = html;

    while results.len() < limit {
        let Some(item_pos) = rest.find("b_algo") else {
            break;
        };
        rest = &rest[item_pos..];
        let next_item = rest["b_algo".len()..]
            .find("b_algo")
            .map(|index| "b_algo".len() + index)
            .unwrap_or(rest.len());
        let block = &rest[..next_item];

        if let Some(result) = parse_bing_result_block(block) {
            results.push(result);
        }
        rest = &rest[next_item..];
    }

    results
}

fn parse_bing_result_block(block: &str) -> Option<SearchResult> {
    let h2_pos = block.find("<h2")?;
    let h2_block = &block[h2_pos..];
    let href = attr_value(h2_block, "href")?;
    let anchor_end = h2_block.find("</a>")?;
    let title_start = h2_block.find('>').map(|index| index + 1)?;
    let title = normalize_text(&strip_html_fragment(&h2_block[title_start..anchor_end]));
    let snippet = extract_bing_snippet(block).unwrap_or_default();

    if title.is_empty() || href.trim().is_empty() {
        return None;
    }

    Some(SearchResult {
        title,
        url: decode_entities(&href),
        snippet,
        score: None,
        source: Some("bing".into()),
    })
}

fn extract_bing_snippet(block: &str) -> Option<String> {
    let p_pos = block.find("<p")?;
    let part = &block[p_pos..];
    let start = part.find('>').map(|index| index + 1)?;
    let end = part[start..].find("</p>").map(|index| start + index)?;
    Some(normalize_text(&strip_html_fragment(&part[start..end])))
}

fn extract_lite_snippet(block: &str) -> Option<String> {
    let marker = "result-snippet";
    let pos = block.find(marker)?;
    let part = &block[pos..];
    let start = part.find('>').map(|index| index + 1)?;
    let end = part[start..].find("</td>").map(|index| start + index)?;
    Some(normalize_text(&strip_html_fragment(&part[start..end])))
}

fn extract_html_snippet(block: &str) -> Option<String> {
    let marker = "result__snippet";
    let pos = block.find(marker)?;
    let part = &block[pos..];
    let start = part.find('>').map(|index| index + 1)?;
    let end = part[start..].find("</").map(|index| start + index)?;
    Some(normalize_text(&strip_html_fragment(&part[start..end])))
}

fn attr_value(input: &str, name: &str) -> Option<String> {
    let marker = format!("{name}=");
    let pos = input.find(&marker)?;
    let value = &input[pos + marker.len()..];
    let quote = value.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &value[quote.len_utf8()..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

fn normalize_duckduckgo_url(url: &str) -> String {
    if let Ok(parsed) = reqwest::Url::parse(url) {
        if let Some(uddg) = parsed
            .query_pairs()
            .find_map(|(key, value)| (key == "uddg").then_some(value.into_owned()))
        {
            return uddg;
        }
        return parsed.to_string();
    }

    if let Ok(parsed) = reqwest::Url::parse(&format!("https://duckduckgo.com{url}"))
        && let Some(uddg) = parsed
            .query_pairs()
            .find_map(|(key, value)| (key == "uddg").then_some(value.into_owned()))
    {
        return uddg;
    }

    url.trim().to_string()
}

fn strip_html_fragment(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    decode_entities(&out)
}

fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use llm_harness_types::UnsupportedEnv;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn make_ctx() -> ToolContext {
        let (tx, _rx) = mpsc::channel(1);
        ToolContext {
            env: Arc::new(UnsupportedEnv::new()),
            run: Arc::new(llm_harness_types::RunContext::new(
                llm_harness_types::RunRequest::default(),
            )),
            abort: CancellationToken::new(),
            tool_use_id: "test-id".into(),
            turn_index: 0,
            assistant_message: Arc::new(llm_harness_types::AssistantMessage {
                kind: llm_harness_types::AssistantMessageKind::FinalAnswer,
                message_id: "test-message".into(),
                turn_id: "test-turn".into(),
                content: vec![],
                usage: None,
                stop_reason: None,
                timestamp: Utc::now(),
                provider: None,
                api: None,
                model: None,
                error_message: None,
            }),
            update_tx: tx,
        }
    }

    #[test]
    fn parses_duckduckgo_html_results() {
        let html = r#"
          <div class="result">
            <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fdocs&amp;rut=abc">
              Example <b>Docs</b>
            </a>
            <a class="result__snippet">Official documentation &amp; examples.</a>
          </div>
        "#;

        let results = parse_duckduckgo_html_results(html, 5);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Example Docs");
        assert_eq!(results[0].url, "https://example.com/docs");
        assert_eq!(results[0].snippet, "Official documentation & examples.");
    }

    #[test]
    fn parses_duckduckgo_lite_results() {
        let html = r#"
          <tr>
            <td><a rel="nofollow" href="https://example.com/docs" class='result-link'>Example <b>Docs</b></a></td>
          </tr>
          <tr>
            <td class='result-snippet'>Official documentation &amp; examples.</td>
          </tr>
        "#;

        let results = parse_duckduckgo_lite_results(html, 5);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Example Docs");
        assert_eq!(results[0].url, "https://example.com/docs");
        assert_eq!(results[0].snippet, "Official documentation & examples.");
        assert_eq!(results[0].source.as_deref(), Some("duckduckgo-lite"));
    }

    #[test]
    fn parses_bing_html_results() {
        let html = r#"
          <li class="b_algo">
            <h2><a href="https://example.com/docs">Example <strong>Docs</strong></a></h2>
            <div class="b_caption"><p>Official documentation &amp; examples.</p></div>
          </li>
        "#;

        let results = parse_bing_html_results(html, 5);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Example Docs");
        assert_eq!(results[0].url, "https://example.com/docs");
        assert_eq!(results[0].snippet, "Official documentation & examples.");
        assert_eq!(results[0].source.as_deref(), Some("bing"));
    }

    #[test]
    fn parses_paid_provider_json_results() {
        let brave = json!({
            "web": {
                "results": [
                    { "title": "Brave Result", "url": "https://example.com/brave", "description": "Brave snippet" }
                ]
            }
        });
        assert_eq!(
            parse_brave_results(&brave, 5)[0],
            SearchResult::new(
                "Brave Result",
                "https://example.com/brave",
                "Brave snippet",
                "brave"
            )
        );

        let tavily = json!({
            "results": [
                { "title": "Tavily Result", "url": "https://example.com/tavily", "content": "Tavily snippet", "score": 0.82 }
            ]
        });
        let tavily_result = &parse_tavily_results(&tavily, 5)[0];
        assert_eq!(tavily_result.source.as_deref(), Some("tavily"));
        assert_eq!(tavily_result.score, Some(0.82));

        let serper = json!({
            "organic": [
                { "title": "Serper Result", "link": "https://example.com/serper", "snippet": "Serper snippet" }
            ]
        });
        assert_eq!(
            parse_serper_results(&serper, 5)[0].source.as_deref(),
            Some("serper")
        );

        let serpapi = json!({
            "organic_results": [
                { "title": "SerpAPI Result", "link": "https://example.com/serpapi", "snippet": "SerpAPI snippet" }
            ]
        });
        assert_eq!(
            parse_serpapi_results(&serpapi, 5)[0].source.as_deref(),
            Some("serpapi")
        );

        let exa = json!({
            "results": [
                { "title": "Exa Result", "url": "https://example.com/exa", "text": "Exa snippet", "score": 0.91 }
            ]
        });
        let exa_result = &parse_exa_results(&exa, 5)[0];
        assert_eq!(exa_result.source.as_deref(), Some("exa"));
        assert_eq!(exa_result.score, Some(0.91));
    }

    #[tokio::test]
    async fn web_search_rejects_empty_query() {
        let tool = WebSearchTool::new();
        let args = serde_json::json!({ "query": "" });
        let err = tool.execute(args, &make_ctx()).await.unwrap_err();
        assert!(err.to_string().contains("query is empty"));
    }

    #[tokio::test]
    #[ignore]
    async fn duckduckgo_live_smoke() {
        let tool = WebSearchTool::new();
        let results = tool.search_duckduckgo("51cgw").await.unwrap();
        eprintln!("{results:#?}");
        assert!(!results.is_empty());
    }
}
