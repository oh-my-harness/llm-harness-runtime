//! `<agent_status>` XML 渲染。

/// 将收集器片段渲染成完整的 `<agent_status>` 消息。
///
/// 输入是各收集器返回的 XML 片段（不含外层标签），
/// 输出是包裹在 `<agent_status>...</agent_status>` 中的完整字符串。
pub fn render_status_xml(fragments: &[String]) -> String {
    if fragments.is_empty() {
        return String::new();
    }
    let mut buf = String::from("<agent_status>\n");
    for fragment in fragments {
        buf.push_str(fragment);
    }
    buf.push_str("</agent_status>");
    buf
}

/// 判断一段文本是否为状态栏消息（以 `<agent_status>` 开头）。
pub fn is_status_text(text: &str) -> bool {
    text.starts_with("<agent_status>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_wrapped_xml() {
        let fragments = vec![
            "  <todo_list>\n    <item id=\"1\">task</item>\n  </todo_list>\n".to_string(),
            "  <environment>\n    <os>linux</os>\n  </environment>\n".to_string(),
        ];
        let xml = render_status_xml(&fragments);
        assert!(xml.starts_with("<agent_status>\n"));
        assert!(xml.ends_with("</agent_status>"));
        assert!(xml.contains("<todo_list>"));
        assert!(xml.contains("<environment>"));
    }

    #[test]
    fn empty_fragments_returns_empty_string() {
        let xml = render_status_xml(&[]);
        assert!(xml.is_empty());
    }

    #[test]
    fn is_status_text_detects_tag() {
        assert!(is_status_text(
            "<agent_status>\n  <todo_list/>\n</agent_status>"
        ));
        assert!(!is_status_text("just a regular user message"));
        assert!(!is_status_text("text mentioning <agent_status> in middle"));
    }
}
