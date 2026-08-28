use futures::future::BoxFuture;
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolResult};
use serde_json::json;
use std::time::Duration;

use crate::text_decode::decode_response_text;
use crate::web_search::WebSearchConfig;

static SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "url": { "type": "string", "description": "HTTP or HTTPS URL to fetch. e.g. \"https://doc.rust-lang.org/book/\" or \"https://example.com/article\"" }
        },
        "required": ["url"]
    })
});

pub struct WebFetchTool {
    config: WebSearchConfig,
    client: reqwest::Client,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self::with_config(WebSearchConfig::default())
    }

    pub fn with_config(config: WebSearchConfig) -> Self {
        let timeout = Duration::from_secs(config.fetch_timeout_secs.clamp(3, 60));
        Self {
            config,
            client: reqwest::Client::builder()
                .timeout(timeout)
                .user_agent("llm-harness-runtime web_fetch")
                .redirect(reqwest::redirect::Policy::limited(5))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn report_duration(&self) -> bool {
        true
    }

    fn description(&self) -> &str {
        "Fetch a web page by URL and extract readable text content for citation-backed answers.\n\n\
         Use when: you have a specific URL from web_search results or a user-provided link; reading the full content of a documentation page, blog post, or article; extracting text from a web page for analysis.\n\
         Don't use for: searching the web when you don't have a URL (use web_search); executing code (use code_exec); fetching binary files, images, or non-HTML content.\n\
         Returns: Page title, extracted text content (truncated to max_fetch_chars), and metadata. HTML is stripped to readable text.\n\n\
         Examples:\n\
         {\"url\": \"https://doc.rust-lang.org/book/ch16-00-concurrency.html\"}\n\
         {\"url\": \"https://github.com/tokio-rs/tokio/blob/main/README.md\"}\n\
         {\"url\": \"https://docs.python.org/3/library/json.html\"}"
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
            let url = args["url"].as_str().unwrap_or("").trim().to_string();
            if url.is_empty() {
                return Err(ToolFailure::invalid_arguments("url is empty"));
            }

            let fetched = self
                .fetch_url(&url)
                .await
                .map_err(|err| ToolFailure::new("web_fetch_failed", err.to_string()))?;
            let text = format_fetch_result(&fetched);
            Ok(ToolResult::ephemeral(
                vec![DataBlock::text(text)],
                format!("Fetched web source {} ({})", fetched.title, fetched.url),
                json!({
                    "url": fetched.url,
                    "title": fetched.title,
                    "description": fetched.description,
                    "content_type": fetched.content_type,
                    "truncated": fetched.truncated,
                    "chars": fetched.text.chars().count(),
                    "sources": [{
                        "index": 1,
                        "kind": "web",
                        "source": fetched.title,
                        "title": fetched.title,
                        "url": fetched.url,
                        "text": "",
                        "score": null,
                    }],
                }),
                false,
            ))
        })
    }
}

impl WebFetchTool {
    async fn fetch_url(&self, url: &str) -> anyhow::Result<FetchedPage> {
        let parsed = reqwest::Url::parse(url)?;
        match parsed.scheme() {
            "http" | "https" => {}
            other => anyhow::bail!("unsupported URL scheme `{other}`; only http/https are allowed"),
        }

        // SSRF protection: reject loopback, link-local, private, and
        // multicast addresses before issuing the request.
        check_ssrf(&parsed)?;

        let response = self.client.get(parsed).send().await?;
        let final_url = response.url().to_string();
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("fetch failed with HTTP {status}");
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !content_type.is_empty()
            && !content_type.contains("text/html")
            && !content_type.contains("text/plain")
            && !content_type.contains("application/xhtml")
        {
            anyhow::bail!("unsupported content type `{content_type}`");
        }

        let headers = response.headers().clone();
        let bytes = response.bytes().await?;
        let body = decode_response_text(&headers, &bytes);
        let page = extract_page_text(
            &body,
            &final_url,
            &content_type,
            self.config.max_fetch_chars,
        );
        Ok(page)
    }
}

/// Reject URLs whose host resolves to a loopback, link-local, private, or
/// multicast IP address — preventing SSRF attacks against internal services.
fn check_ssrf(url: &reqwest::Url) -> anyhow::Result<()> {
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("URL has no host"))?;

    // Strip IPv6 brackets if present — url::host_str() may return `[::1]`.
    let host = host.trim_start_matches('[').trim_end_matches(']');

    // If the host is already an IP literal, check it directly.
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if is_forbidden_ip(&ip) {
            anyhow::bail!(
                "URL host `{host}` is a forbidden address (loopback/link-local/private/multicast)"
            );
        }
        return Ok(());
    }

    // For hostnames, resolve to IPs and check each one.
    // Note: this is a best-effort check — a DNS rebinding attack could still
    // return a public IP here that later resolves to a private address.
    // The redirect policy (limited to 5 hops) mitigates but does not
    // eliminate this risk.
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs = match std::net::ToSocketAddrs::to_socket_addrs(&(host, port)) {
        Ok(addrs) => addrs.collect::<Vec<_>>(),
        Err(_) => {
            // DNS resolution failed — let reqwest handle the error.
            return Ok(());
        }
    };
    for addr in addrs {
        if is_forbidden_ip(&addr.ip()) {
            anyhow::bail!(
                "URL host `{host}` resolves to forbidden address {} (loopback/link-local/private/multicast)",
                addr.ip()
            );
        }
    }
    Ok(())
}

fn is_forbidden_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_link_local()
                || v4.is_private()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_documentation()
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_multicast()
                || v6.is_unspecified()
                || is_ipv6_unicast_link_local(v6)
                || is_ipv6_unique_local(v6)
        }
    }
}

/// Check if an IPv6 address is a unique-local address (fc00::/7).
fn is_ipv6_unique_local(addr: &std::net::Ipv6Addr) -> bool {
    let octets = addr.octets();
    (octets[0] & 0xfe) == 0xfc
}

/// Check if an IPv6 address is a unicast link-local address (fe80::/10).
fn is_ipv6_unicast_link_local(addr: &std::net::Ipv6Addr) -> bool {
    let octets = addr.octets();
    octets[0] == 0xfe && (octets[1] & 0xc0) == 0x80
}

#[derive(Clone, Debug, PartialEq)]
struct FetchedPage {
    title: String,
    description: String,
    url: String,
    content_type: String,
    text: String,
    truncated: bool,
}

fn extract_page_text(body: &str, url: &str, content_type: &str, max_chars: usize) -> FetchedPage {
    let title = html_tag_text(body, "title").unwrap_or_else(|| url.to_string());
    let description = html_meta_content(body, "description").unwrap_or_default();
    let cleaned = if content_type.contains("text/plain") {
        normalize_whitespace(body)
    } else {
        normalize_whitespace(&strip_html(body))
    };
    let max_chars = max_chars.clamp(1_000, 60_000);
    let (text, truncated) = truncate_chars(&cleaned, max_chars);

    FetchedPage {
        title: decode_entities(&title),
        description: decode_entities(&description),
        url: url.to_string(),
        content_type: content_type.to_string(),
        text,
        truncated,
    }
}

fn format_fetch_result(page: &FetchedPage) -> String {
    let mut lines = vec![
        format!("[WEB] Fetched: {}", page.title),
        format!("URL: {}", page.url),
    ];
    if !page.description.trim().is_empty() {
        lines.push(format!("Description: {}", page.description));
    }
    if page.truncated {
        lines.push("Note: content was truncated.".into());
    }
    lines.push(format!("Content:\n{}", page.text));
    lines.join("\n")
}

fn strip_html(input: &str) -> String {
    let without_scripts = remove_tag_block(input, "script");
    let without_styles = remove_tag_block(&without_scripts, "style");
    let without_svg = remove_tag_block(&without_styles, "svg");

    let mut out = String::with_capacity(without_svg.len());
    let mut in_tag = false;
    for ch in without_svg.chars() {
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

fn remove_tag_block(input: &str, tag: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    let open = format!("<{tag}");
    let close = format!("</{tag}>");

    loop {
        let lower = rest.to_ascii_lowercase();
        let Some(start) = lower.find(&open) else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let after_start = &rest[start..];
        let after_lower = after_start.to_ascii_lowercase();
        let Some(end) = after_lower.find(&close) else {
            break;
        };
        rest = &after_start[end + close.len()..];
    }

    out
}

fn html_tag_text(input: &str, tag: &str) -> Option<String> {
    let lower = input.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let start = lower.find(&open)?;
    let after_open = input[start..].find('>')? + start + 1;
    let end = lower[after_open..].find(&close)? + after_open;
    Some(normalize_whitespace(&strip_html(&input[after_open..end])))
}

fn html_meta_content(input: &str, name: &str) -> Option<String> {
    for part in input.split('<') {
        let lower = part.to_ascii_lowercase();
        if !lower.starts_with("meta") || !lower.contains(&format!("name=\"{name}\"")) {
            continue;
        }
        let content_index = lower.find("content=")?;
        let value = &part[content_index + "content=".len()..];
        return quoted_attr_value(value).map(|text| text.to_string());
    }
    None
}

fn quoted_attr_value(value: &str) -> Option<&str> {
    let mut chars = value.chars();
    let quote = chars.next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &value[quote.len_utf8()..];
    let end = rest.find(quote)?;
    Some(&rest[..end])
}

fn normalize_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_string(), false);
    }
    (text.chars().take(max_chars).collect(), true)
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

    #[test]
    fn ssrf_rejects_loopback_ipv4() {
        let url = reqwest::Url::parse("http://127.0.0.1:8080/").unwrap();
        assert!(check_ssrf(&url).is_err());
    }

    #[test]
    fn ssrf_rejects_link_local_ipv4() {
        let url = reqwest::Url::parse("http://169.254.169.254/latest/").unwrap();
        assert!(check_ssrf(&url).is_err());
    }

    #[test]
    fn ssrf_rejects_private_ipv4() {
        let url = reqwest::Url::parse("http://10.0.0.1/").unwrap();
        assert!(check_ssrf(&url).is_err());
        let url = reqwest::Url::parse("http://192.168.1.1/").unwrap();
        assert!(check_ssrf(&url).is_err());
        let url = reqwest::Url::parse("http://172.16.0.1/").unwrap();
        assert!(check_ssrf(&url).is_err());
    }

    #[test]
    fn ssrf_rejects_loopback_ipv6() {
        let url = reqwest::Url::parse("http://[::1]:6379/").unwrap();
        assert!(check_ssrf(&url).is_err());
    }

    #[test]
    fn ssrf_allows_public_ipv4() {
        let url = reqwest::Url::parse("http://8.8.8.8/").unwrap();
        assert!(check_ssrf(&url).is_ok());
    }
}
