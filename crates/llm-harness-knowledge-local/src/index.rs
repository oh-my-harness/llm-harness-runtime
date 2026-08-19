use std::collections::{BTreeMap, HashMap};

use crate::ParsedDocument;

#[derive(Debug, Clone)]
pub struct IndexedDocument {
    pub item_id: String,
    pub parsed: ParsedDocument,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DocumentSearchMatch {
    pub item_id: String,
    pub score: f32,
    pub section_ids: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Bm25DocumentSearchIndex {
    documents: Vec<IndexedDocument>,
    term_frequencies: Vec<HashMap<String, usize>>,
    document_frequencies: BTreeMap<String, usize>,
    average_length: f32,
}

impl Bm25DocumentSearchIndex {
    pub fn build(documents: Vec<IndexedDocument>) -> Self {
        let mut term_frequencies = Vec::with_capacity(documents.len());
        let mut document_frequencies = BTreeMap::new();
        let mut total_terms = 0;
        for document in &documents {
            let frequencies = frequencies(&document.parsed.searchable_text);
            total_terms += frequencies.values().sum::<usize>();
            for term in frequencies.keys() {
                *document_frequencies.entry(term.clone()).or_default() += 1;
            }
            term_frequencies.push(frequencies);
        }
        let average_length = if documents.is_empty() {
            0.0
        } else {
            total_terms as f32 / documents.len() as f32
        };
        Self {
            documents,
            term_frequencies,
            document_frequencies,
            average_length,
        }
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<DocumentSearchMatch> {
        let terms = tokenize(query);
        let count = self.documents.len() as f32;
        let mut matches = self
            .documents
            .iter()
            .zip(&self.term_frequencies)
            .filter_map(|(document, frequencies)| {
                let length = frequencies.values().sum::<usize>() as f32;
                let mut score = 0.0;
                for term in &terms {
                    let tf = *frequencies.get(term).unwrap_or(&0) as f32;
                    if tf == 0.0 {
                        continue;
                    }
                    let df = *self.document_frequencies.get(term).unwrap_or(&0) as f32;
                    let idf = ((count - df + 0.5) / (df + 0.5) + 1.0).ln();
                    let normalization =
                        tf + 1.2 * (1.0 - 0.75 + 0.75 * length / self.average_length.max(1.0));
                    score += idf * (tf * 2.2) / normalization;
                }
                (score > 0.0).then(|| DocumentSearchMatch {
                    item_id: document.item_id.clone(),
                    score,
                    section_ids: best_sections(&document.parsed, &terms),
                })
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.item_id.cmp(&right.item_id))
        });
        matches.truncate(limit);
        matches
    }
}

fn best_sections(document: &ParsedDocument, terms: &[String]) -> Vec<String> {
    let mut sections = document
        .sections
        .iter()
        .filter_map(|section| {
            let frequencies = frequencies(&format!(
                "{}\n{}",
                section.title.as_deref().unwrap_or(""),
                section.text
            ));
            let score = terms
                .iter()
                .map(|term| frequencies.get(term).copied().unwrap_or(0))
                .sum::<usize>();
            (score > 0).then(|| (section.id.clone(), score))
        })
        .collect::<Vec<_>>();
    sections.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    sections.into_iter().take(3).map(|value| value.0).collect()
}

fn frequencies(text: &str) -> HashMap<String, usize> {
    let mut result = HashMap::new();
    for token in tokenize(text) {
        *result.entry(token).or_default() += 1;
    }
    result
}

pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut ascii = String::new();
    for character in text.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            ascii.push(character);
        } else {
            if !ascii.is_empty() {
                tokens.push(std::mem::take(&mut ascii));
            }
            if character.is_alphanumeric() {
                tokens.push(character.to_string());
            }
        }
    }
    if !ascii.is_empty() {
        tokens.push(ascii);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DocumentSection;

    fn document(id: &str, title: &str, text: &str) -> IndexedDocument {
        IndexedDocument {
            item_id: id.into(),
            parsed: ParsedDocument {
                title: Some(title.into()),
                searchable_text: format!("{title}\n{text}"),
                text: text.into(),
                sections: vec![DocumentSection {
                    id: "body".into(),
                    title: None,
                    text: text.into(),
                }],
                mime_type: "text/plain".into(),
            },
        }
    }

    #[test]
    fn bm25_ranks_matching_document_and_section() {
        let index = Bm25DocumentSearchIndex::build(vec![
            document("refund", "Refunds", "refund requests within thirty days"),
            document("shipping", "Shipping", "delivery and tracking"),
        ]);
        let results = index.search("refund days", 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].item_id, "refund");
        assert_eq!(results[0].section_ids, vec!["body"]);
    }

    #[test]
    fn tokenizer_handles_words_and_cjk_without_backend_specific_analysis() {
        assert_eq!(
            tokenize("Refund政策 30天"),
            vec!["refund", "政", "策", "30", "天"]
        );
    }
}
