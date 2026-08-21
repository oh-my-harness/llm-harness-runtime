//! 多层兜底的 JSON 对象解析。参考 DeepTutor `extract_json_object`。

use serde_json::Value;

/// 从模型输出文本里提取第一个可用的 JSON object。
///
/// 1. \`\`\`json 代码块逐个尝试。
/// 2. 原始文本直接解析。
/// 3. 从首个 `{` 位置用 Deserializer 贪婪解析。
/// 4. 最外层 `{...}` 子串再解析。
/// 5. 全失败 → `None`。
///
/// 输入超过 `MAX_INPUT_BYTES` 时直接返回 None，防止恶意构造的超大输出
/// 导致解析 DoS（旧 submit_tool.rs 有 256KB 限制，此处保留同等防护）。
const MAX_INPUT_BYTES: usize = 256 * 1024;

pub(crate) fn extract_json_object(text: &str) -> Option<Value> {
    if text.len() > MAX_INPUT_BYTES {
        return None;
    }
    let raw = text.trim();
    if raw.is_empty() {
        return None;
    }

    // 1. fenced 代码块
    for candidate in extract_fenced_blocks(raw) {
        if let Ok(parsed) = serde_json::from_str::<Value>(&candidate)
            && parsed.is_object()
        {
            return Some(parsed);
        }
        if let Some(parsed) = decode_first_json_object(&candidate) {
            return Some(parsed);
        }
    }

    // 2. 原始文本
    if let Ok(parsed) = serde_json::from_str::<Value>(raw)
        && parsed.is_object()
    {
        return Some(parsed);
    }
    if let Some(parsed) = decode_first_json_object(raw) {
        return Some(parsed);
    }

    // 3 & 4. 最外层 {...} 子串
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end > start {
        let snippet = &raw[start..=end];
        if let Ok(parsed) = serde_json::from_str::<Value>(snippet)
            && parsed.is_object()
        {
            return Some(parsed);
        }
        if let Some(parsed) = decode_first_json_object(snippet) {
            return Some(parsed);
        }
    }

    None
}

fn extract_fenced_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        let after_fence = &rest[start + 3..];
        // skip optional language tag line
        let content_start = match after_fence.find('\n') {
            Some(n) => n + 1,
            None => 0,
        };
        let content = &after_fence[content_start..];
        match content.find("```") {
            Some(close) => {
                blocks.push(content[..close].to_string());
                rest = &content[close + 3..];
            }
            None => break,
        }
    }
    blocks
}

fn decode_first_json_object(text: &str) -> Option<Value> {
    let stripped = text.trim_start();
    if stripped.is_empty() {
        return None;
    }
    // 候选起始位置：开头 0 + 首个 `{`。去重避免无 `{` 时重复从 0 解析。
    let brace = stripped.find('{').unwrap_or(0);
    let mut starts: Vec<usize> = vec![0, brace];
    starts.sort();
    starts.dedup();
    for start in starts {
        if start >= stripped.len() {
            continue;
        }
        let slice = &stripped[start..];
        let mut de = serde_json::Deserializer::from_str(slice).into_iter::<Value>();
        if let Some(Ok(parsed)) = de.next()
            && parsed.is_object()
        {
            return Some(parsed);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_json() {
        let v = extract_json_object(r#"{"a":1,"b":2}"#).unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"], 2);
    }

    #[test]
    fn parses_fenced_json_block() {
        let v = extract_json_object("result:\n```json\n{\"x\":true}\n```\n").unwrap();
        assert_eq!(v["x"], true);
    }

    #[test]
    fn parses_fenced_without_lang_tag() {
        let v = extract_json_object("```\n{\"y\":3}\n```").unwrap();
        assert_eq!(v["y"], 3);
    }

    #[test]
    fn parses_json_with_leading_text() {
        let v = extract_json_object(r#"结果：{"score":0.9}"#).unwrap();
        assert_eq!(v["score"], 0.9);
    }

    #[test]
    fn parses_json_with_trailing_text() {
        let v = extract_json_object(r#"{"k":"v"} 以上"#).unwrap();
        assert_eq!(v["k"], "v");
    }

    #[test]
    fn returns_none_for_no_json() {
        assert!(extract_json_object("plain text").is_none());
    }

    #[test]
    fn returns_none_for_empty() {
        assert!(extract_json_object("").is_none());
        assert!(extract_json_object("   \n\t ").is_none());
    }

    #[test]
    fn returns_none_for_non_object_json() {
        assert!(extract_json_object("[1,2,3]").is_none());
        assert!(extract_json_object("42").is_none());
        assert!(extract_json_object("\"string\"").is_none());
        assert!(extract_json_object("true").is_none());
        assert!(extract_json_object("null").is_none());
    }

    #[test]
    fn handles_nested_braces() {
        let v = extract_json_object(r#"r: {"outer":{"inner":1}}"#).unwrap();
        assert_eq!(v["outer"]["inner"], 1);
    }

    // ── 额外覆盖：多层 fallback 边界 ────────────────────────────────────────

    #[test]
    fn parses_first_fenced_block_when_multiple() {
        let input = "```json\n{\"first\":1}\n```\n```json\n{\"second\":2}\n```";
        let v = extract_json_object(input).unwrap();
        assert_eq!(v["first"], 1);
    }

    #[test]
    fn parses_fenced_with_prose_inside_block() {
        // fenced block 里 json 前后有 prose：靠 decode_first_json_object 兜底
        let input = "```json\nThe answer:\n{\"deep\":42}\n```";
        let v = extract_json_object(input).unwrap();
        assert_eq!(v["deep"], 42);
    }

    #[test]
    fn parses_json_with_leading_and_trailing_text() {
        let v = extract_json_object(r#"before {"m": -7} after"#).unwrap();
        assert_eq!(v["m"], -7);
    }

    #[test]
    fn parses_nested_object_array_values() {
        let v = extract_json_object(
            r#"```json
            {"items":[{"id":1},{"id":2}],"count":2}
            ```"#,
        )
        .unwrap();
        assert_eq!(v["count"], 2);
        assert_eq!(v["items"][0]["id"], 1);
        assert_eq!(v["items"][1]["id"], 2);
    }

    #[test]
    fn parses_unicode_values() {
        let v = extract_json_object(r#"{"name":"你好世界"}"#).unwrap();
        assert_eq!(v["name"], "你好世界");
    }

    #[test]
    fn returns_none_when_only_array_in_fenced() {
        // fenced block 里只有数组 → not an object → None
        assert!(extract_json_object("```\n[1,2,3]\n```").is_none());
    }

    #[test]
    fn handles_deeply_nested_object() {
        let v = extract_json_object(r#"{"a":{"b":{"c":{"d":5}}}}"#).unwrap();
        assert_eq!(v["a"]["b"]["c"]["d"], 5);
    }

    #[test]
    fn parses_json_with_escaped_quotes() {
        let v = extract_json_object(r#"{"q":"he said \"hi\""}"#).unwrap();
        assert_eq!(v["q"], "he said \"hi\"");
    }

    #[test]
    fn returns_none_for_unclosed_brace() {
        assert!(extract_json_object(r#"{"a":1"#).is_none());
    }

    #[test]
    fn trims_whitespace_around_json() {
        let v = extract_json_object("  \n  {\"k\":1}  \n  ").unwrap();
        assert_eq!(v["k"], 1);
    }

    #[test]
    fn fallback_to_brace_substring_when_fenced_malformed() {
        // fenced 块不闭合 → 不被识别为 fenced → 走原始文本 / brace substring 路径
        let input = "pre {\"z\":9} post";
        let v = extract_json_object(input).unwrap();
        assert_eq!(v["z"], 9);
    }

    #[test]
    fn picks_first_object_among_multiple_braces() {
        // 多个 {...} 子串：最外层 `{` 到最外层 `}`，但内部前段 JSON 应先成功
        let v = extract_json_object(r#"{"a":1} trailing {"b":2}"#).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn parses_json_with_trailing_comma_via_decoder_fallback() {
        // serde_json 默认拒绝 trailing comma，但 raw_decode 也无法解析非法 JSON
        // → None（验证不返回半解析结果）
        assert!(extract_json_object(r#"{"a":1,}"#).is_none());
    }

    #[test]
    fn handles_only_opening_brace() {
        assert!(extract_json_object("{").is_none());
    }

    #[test]
    fn handles_only_closing_brace() {
        assert!(extract_json_object("}").is_none());
    }
    #[test]
    fn rejects_oversized_input() {
        let big = "x".repeat(MAX_INPUT_BYTES + 1);
        assert!(extract_json_object(&big).is_none());
        // 边界值：刚好等于上限仍可处理（空 JSON 不合法，返回 None 但不是因为大小）
        let boundary = "{".repeat(MAX_INPUT_BYTES);
        assert!(extract_json_object(&boundary).is_none());
    }
}
