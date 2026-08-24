use encoding_rs::{Encoding, UTF_8};
use reqwest::header::{CONTENT_TYPE, HeaderMap};

pub(crate) fn decode_response_text(headers: &HeaderMap, bytes: &[u8]) -> String {
    let mut labels = Vec::new();
    if let Some(label) = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(charset_from_content_type)
    {
        labels.push(label);
    }
    if let Some(label) = charset_from_html_meta(bytes) {
        labels.push(label);
    }
    labels.extend([
        "utf-8".to_string(),
        "gb18030".to_string(),
        "gbk".to_string(),
        "big5".to_string(),
    ]);

    let mut best: Option<(usize, String)> = None;
    for label in labels {
        let encoding = Encoding::for_label(label.trim().as_bytes()).unwrap_or(UTF_8);
        let (decoded, _, had_errors) = encoding.decode(bytes);
        let text = decoded.into_owned();
        let score = mojibake_score(&text) + usize::from(had_errors) * 200;
        match &best {
            Some((best_score, _)) if *best_score <= score => {}
            _ => best = Some((score, text)),
        }
    }

    best.map(|(_, text)| text)
        .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned())
}

fn charset_from_content_type(content_type: &str) -> Option<String> {
    content_type
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("charset="))
        .map(|value| value.trim_matches(['"', '\'']).trim().to_string())
        .filter(|value| !value.is_empty())
}

fn charset_from_html_meta(bytes: &[u8]) -> Option<String> {
    let prefix_len = bytes.len().min(4096);
    let prefix = String::from_utf8_lossy(&bytes[..prefix_len]).to_ascii_lowercase();
    let charset_pos = prefix.find("charset")?;
    let after = &prefix[charset_pos + "charset".len()..];
    let equals_pos = after.find('=')?;
    let mut value = after[equals_pos + 1..].trim_start();
    if let Some(stripped) = value.strip_prefix(['"', '\'']) {
        value = stripped;
    }
    let label: String = value
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        .collect();
    (!label.is_empty()).then_some(label)
}

fn mojibake_score(text: &str) -> usize {
    let replacements = text.matches('\u{FFFD}').count() * 80;
    let controls = text
        .chars()
        .filter(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
        .count()
        * 20;
    let suspicious = [
        "Ã", "Â", "â€", "鈥", "鍙", "涓", "绯", "鐗", "浣", "妯", "笁", "簧",
    ]
    .iter()
    .map(|needle| text.matches(needle).count())
    .sum::<usize>()
        * 12;

    replacements + controls + suspicious
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn decodes_gbk_html_from_meta_charset() {
        let html = "<html><head><meta charset=\"gbk\"></head><body>吉林大学</body></html>";
        let (bytes, _, _) = Encoding::for_label(b"gbk").unwrap().encode(html);
        let headers = HeaderMap::new();

        let decoded = decode_response_text(&headers, bytes.as_ref());

        assert!(decoded.contains("吉林大学"));
    }

    #[test]
    fn prefers_utf8_when_header_claims_gbk_but_text_is_utf8() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=gbk"),
        );
        let bytes = "系统测试：吉林大学".as_bytes();

        let decoded = decode_response_text(&headers, bytes);

        assert_eq!(decoded, "系统测试：吉林大学");
    }
}
