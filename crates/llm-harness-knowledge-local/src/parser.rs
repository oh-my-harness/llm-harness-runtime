use std::{collections::BTreeMap, path::Path, sync::Arc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentSection {
    pub id: String,
    pub title: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDocument {
    pub title: Option<String>,
    pub searchable_text: String,
    pub text: String,
    pub sections: Vec<DocumentSection>,
    pub mime_type: String,
}

#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum DocumentParseError {
    #[error("document is not valid UTF-8")]
    InvalidUtf8,
    #[error("document format is unsupported")]
    UnsupportedFormat,
}

pub trait DocumentParser: Send + Sync {
    fn extensions(&self) -> &[&str];
    fn parse(&self, path: &Path, bytes: &[u8]) -> Result<ParsedDocument, DocumentParseError>;
}

pub struct DocumentParserRegistry {
    parsers: BTreeMap<String, Arc<dyn DocumentParser>>,
}

impl Default for DocumentParserRegistry {
    fn default() -> Self {
        let mut registry = Self {
            parsers: BTreeMap::new(),
        };
        registry.register(MarkdownDocumentParser);
        registry.register(TextDocumentParser);
        registry
    }
}

impl DocumentParserRegistry {
    pub fn new() -> Self {
        Self {
            parsers: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, parser: impl DocumentParser + 'static) {
        let parser: Arc<dyn DocumentParser> = Arc::new(parser);
        for extension in parser.extensions() {
            self.parsers
                .insert(extension.to_ascii_lowercase(), parser.clone());
        }
    }

    pub fn supports(&self, extension: &str) -> bool {
        self.parsers.contains_key(&extension.to_ascii_lowercase())
    }

    pub fn parse(&self, path: &Path, bytes: &[u8]) -> Result<ParsedDocument, DocumentParseError> {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .map(str::to_ascii_lowercase)
            .ok_or(DocumentParseError::UnsupportedFormat)?;
        self.parsers
            .get(&extension)
            .ok_or(DocumentParseError::UnsupportedFormat)?
            .parse(path, bytes)
    }
}

pub struct MarkdownDocumentParser;

impl DocumentParser for MarkdownDocumentParser {
    fn extensions(&self) -> &[&str] {
        &["md", "markdown"]
    }

    fn parse(&self, path: &Path, bytes: &[u8]) -> Result<ParsedDocument, DocumentParseError> {
        let text = std::str::from_utf8(bytes).map_err(|_| DocumentParseError::InvalidUtf8)?;
        let (frontmatter, body) = split_frontmatter(text);
        let fallback_title = path.file_stem().and_then(|value| value.to_str());
        let frontmatter_title = frontmatter.iter().find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.trim().eq_ignore_ascii_case("title"))
                .map(|(_, value)| value.trim().trim_matches('"').to_string())
        });

        let mut sections = Vec::new();
        let mut heading_path: Vec<String> = Vec::new();
        let mut current_title = None;
        let mut current_id = "preamble".to_string();
        let mut current_lines = Vec::new();
        let mut seen_ids = BTreeMap::<String, usize>::new();
        let mut first_heading = None;

        for line in body.lines() {
            if let Some((level, heading)) = markdown_heading(line) {
                push_section(
                    &mut sections,
                    &current_id,
                    current_title.take(),
                    &current_lines,
                );
                current_lines.clear();
                first_heading.get_or_insert_with(|| heading.to_string());
                heading_path.truncate(level.saturating_sub(1));
                let segment = slug(heading);
                let base = if heading_path.is_empty() {
                    segment.clone()
                } else {
                    format!("{}/{}", heading_path.join("/"), segment)
                };
                let occurrence = seen_ids.entry(base.clone()).or_default();
                *occurrence += 1;
                let unique_segment = if *occurrence == 1 {
                    segment
                } else {
                    format!("{segment}-{}", occurrence)
                };
                heading_path.push(unique_segment);
                current_id = heading_path.join("/");
                current_title = Some(heading.to_string());
            } else {
                current_lines.push(line);
            }
        }
        push_section(&mut sections, &current_id, current_title, &current_lines);

        let title = frontmatter_title
            .or(first_heading)
            .or_else(|| fallback_title.map(str::to_string));
        let searchable_text = format!(
            "{}\n{}\n{}",
            title.as_deref().unwrap_or(""),
            frontmatter.join("\n"),
            body
        );
        Ok(ParsedDocument {
            title,
            searchable_text,
            text: body.to_string(),
            sections,
            mime_type: "text/markdown".into(),
        })
    }
}

pub struct TextDocumentParser;

impl DocumentParser for TextDocumentParser {
    fn extensions(&self) -> &[&str] {
        &["txt"]
    }

    fn parse(&self, path: &Path, bytes: &[u8]) -> Result<ParsedDocument, DocumentParseError> {
        let text = std::str::from_utf8(bytes).map_err(|_| DocumentParseError::InvalidUtf8)?;
        let title = path
            .file_stem()
            .and_then(|value| value.to_str())
            .map(str::to_string);
        Ok(ParsedDocument {
            title: title.clone(),
            searchable_text: format!("{}\n{text}", title.as_deref().unwrap_or("")),
            text: text.to_string(),
            sections: vec![DocumentSection {
                id: "document".into(),
                title,
                text: text.to_string(),
            }],
            mime_type: "text/plain".into(),
        })
    }
}

fn split_frontmatter(text: &str) -> (Vec<&str>, &str) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (vec![], text);
    };
    let Some(end) = rest.find("\n---\n") else {
        return (vec![], text);
    };
    (rest[..end].lines().collect(), &rest[end + 5..])
}

fn markdown_heading(line: &str) -> Option<(usize, &str)> {
    let hashes = line.chars().take_while(|value| *value == '#').count();
    if !(1..=6).contains(&hashes) || !line[hashes..].starts_with(' ') {
        return None;
    }
    let heading = line[hashes + 1..].trim();
    (!heading.is_empty()).then_some((hashes, heading))
}

fn slug(value: &str) -> String {
    let mut result = String::new();
    let mut separator = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_alphanumeric() {
            if separator && !result.is_empty() {
                result.push('-');
            }
            result.push(character);
            separator = false;
        } else {
            separator = true;
        }
    }
    if result.is_empty() {
        "section".into()
    } else {
        result
    }
}

fn push_section(
    sections: &mut Vec<DocumentSection>,
    id: &str,
    title: Option<String>,
    lines: &[&str],
) {
    let text = lines.join("\n").trim().to_string();
    if title.is_some() || !text.is_empty() {
        sections.push(DocumentSection {
            id: id.to_string(),
            title,
            text,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_extracts_frontmatter_title_and_stable_heading_paths() {
        let document = MarkdownDocumentParser
            .parse(
                Path::new("policy.md"),
                b"---\ntitle: Refund Rules\nowner: support\n---\n# Eligibility\nIntro\n## Window\nThirty days\n# Eligibility\nOther\n## Window\nFourteen days",
            )
            .unwrap();

        assert_eq!(document.title.as_deref(), Some("Refund Rules"));
        assert!(document.searchable_text.contains("owner: support"));
        assert_eq!(
            document
                .sections
                .iter()
                .map(|section| section.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "eligibility",
                "eligibility/window",
                "eligibility-2",
                "eligibility-2/window"
            ]
        );
    }

    #[test]
    fn text_parser_exposes_one_document_section() {
        let document = TextDocumentParser
            .parse(Path::new("notes.txt"), b"hello\nworld")
            .unwrap();
        assert_eq!(document.title.as_deref(), Some("notes"));
        assert_eq!(document.sections[0].id, "document");
        assert_eq!(document.mime_type, "text/plain");
    }
}
