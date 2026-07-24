pub(crate) fn sanitize_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() || sanitized.chars().all(|c| c == '_') {
        "unnamed".to_owned()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_name_preserves_alphanumeric() {
        assert_eq!(sanitize_name("my_server-1"), "my_server_1");
    }

    #[test]
    fn sanitize_name_empty_returns_unnamed() {
        assert_eq!(sanitize_name(""), "unnamed");
    }

    #[test]
    fn sanitize_name_all_special_returns_unnamed() {
        assert_eq!(sanitize_name("---"), "unnamed");
    }

    #[test]
    fn sanitize_name_preserves_underscores() {
        assert_eq!(sanitize_name("already_ok"), "already_ok");
    }
}
