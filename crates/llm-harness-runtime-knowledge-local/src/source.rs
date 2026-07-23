use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    sync::{Arc, RwLock},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use futures::future::BoxFuture;
use hmac::{Hmac, Mac};
use llm_harness_runtime_knowledge::{
    ContentSelector, FreshnessClass, KnowledgeAccessControl, KnowledgeAction, KnowledgeCapability,
    KnowledgeContent, KnowledgeError, KnowledgeHit, KnowledgeReadRequest, KnowledgeRef,
    KnowledgeRequestContext, KnowledgeResourceRef, KnowledgeSource, KnowledgeSourceDescriptor,
    SectionRef, SourceSearchPage, SourceSearchRequest,
};
use llm_harness_types::DataBlock;
use sha2::Sha256;
use tokio_util::sync::CancellationToken;

use crate::{
    Bm25DocumentSearchIndex, DocumentFileSystem, DocumentFileSystemError, DocumentParserRegistry,
    IndexedDocument, ParsedDocument, content_revision,
};

type HmacSha256 = Hmac<Sha256>;

pub struct LocalDocumentSourceConfig {
    pub source_id: String,
    pub name: String,
    pub description: String,
    pub domains: Vec<String>,
    pub roots: Vec<DocumentRootConfig>,
    pub source_secret: Vec<u8>,
    pub max_document_bytes: usize,
}

#[derive(Clone)]
pub struct DocumentRootConfig {
    pub id: String,
    pub path: PathBuf,
}

#[derive(thiserror::Error, Debug)]
pub enum LocalDocumentSourceError {
    #[error("invalid local document source configuration: {0}")]
    InvalidConfig(String),
    #[error("document path is outside the configured roots")]
    RootViolation,
    #[error("document is not a regular supported file")]
    UnsupportedFile,
    #[error("document exceeds the configured size limit")]
    TooLarge,
    #[error("document filesystem failed")]
    FileSystem(#[from] DocumentFileSystemError),
    #[error("document parsing failed")]
    Parse(#[from] crate::DocumentParseError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentRefreshReport {
    pub indexed: usize,
    pub skipped: usize,
}

#[derive(Clone)]
struct CatalogEntry {
    item_id: String,
    canonical_path: PathBuf,
    revision: String,
    parsed: ParsedDocument,
}

#[derive(Default)]
struct CatalogSnapshot {
    entries: BTreeMap<String, CatalogEntry>,
    index: Bm25DocumentSearchIndex,
}

struct DocumentRootPolicy {
    roots: Vec<CanonicalDocumentRoot>,
    max_document_bytes: usize,
}

struct CanonicalDocumentRoot {
    id: String,
    path: PathBuf,
}

impl DocumentRootPolicy {
    async fn create(
        roots: Vec<DocumentRootConfig>,
        max_document_bytes: usize,
        file_system: &dyn DocumentFileSystem,
    ) -> Result<Self, LocalDocumentSourceError> {
        if roots.is_empty() {
            return Err(LocalDocumentSourceError::InvalidConfig(
                "at least one document root is required".into(),
            ));
        }
        if max_document_bytes == 0 {
            return Err(LocalDocumentSourceError::InvalidConfig(
                "max_document_bytes must be greater than zero".into(),
            ));
        }
        let mut canonical_roots = Vec::with_capacity(roots.len());
        let mut root_ids = BTreeSet::new();
        for root in roots {
            if root.id.trim().is_empty() || !root_ids.insert(root.id.clone()) {
                return Err(LocalDocumentSourceError::InvalidConfig(
                    "document root IDs must be non-empty and unique".into(),
                ));
            }
            let canonical = file_system.canonicalize(&root.path).await?;
            let metadata = file_system.metadata(&canonical).await?;
            if !metadata.is_dir {
                return Err(LocalDocumentSourceError::InvalidConfig(
                    "every document root must be a directory".into(),
                ));
            }
            if canonical_roots
                .iter()
                .any(|existing: &CanonicalDocumentRoot| existing.path == canonical)
            {
                return Err(LocalDocumentSourceError::InvalidConfig(
                    "document roots must resolve to unique directories".into(),
                ));
            }
            canonical_roots.push(CanonicalDocumentRoot {
                id: root.id,
                path: canonical,
            });
        }
        Ok(Self {
            roots: canonical_roots,
            max_document_bytes,
        })
    }

    async fn validate(
        &self,
        candidate: &Path,
        file_system: &dyn DocumentFileSystem,
    ) -> Result<(String, PathBuf, PathBuf), LocalDocumentSourceError> {
        if candidate
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(LocalDocumentSourceError::RootViolation);
        }
        let canonical = file_system.canonicalize(candidate).await?;
        let root = self
            .roots
            .iter()
            .find(|root| canonical.starts_with(&root.path))
            .ok_or(LocalDocumentSourceError::RootViolation)?;
        let metadata = file_system.metadata(&canonical).await?;
        if !metadata.is_file {
            return Err(LocalDocumentSourceError::UnsupportedFile);
        }
        if metadata.len > self.max_document_bytes as u64 {
            return Err(LocalDocumentSourceError::TooLarge);
        }
        let relative = canonical
            .strip_prefix(&root.path)
            .map_err(|_| LocalDocumentSourceError::RootViolation)?
            .to_path_buf();
        Ok((root.id.clone(), canonical, relative))
    }
}

pub struct LocalDocumentSource {
    descriptor: KnowledgeSourceDescriptor,
    file_system: Arc<dyn DocumentFileSystem>,
    parsers: DocumentParserRegistry,
    access_control: Arc<KnowledgeAccessControl>,
    root_policy: DocumentRootPolicy,
    source_secret: Vec<u8>,
    catalog: RwLock<Arc<CatalogSnapshot>>,
}

impl LocalDocumentSource {
    pub async fn create(
        config: LocalDocumentSourceConfig,
        file_system: Arc<dyn DocumentFileSystem>,
        parsers: DocumentParserRegistry,
        access_control: Arc<KnowledgeAccessControl>,
    ) -> Result<Self, LocalDocumentSourceError> {
        if !is_safe_identifier(&config.source_id) {
            return Err(LocalDocumentSourceError::InvalidConfig(
                "source_id must use ASCII letters, digits, '.', '_' or '-'".into(),
            ));
        }
        if config.source_secret.len() < 32 {
            return Err(LocalDocumentSourceError::InvalidConfig(
                "source_secret must contain at least 32 bytes".into(),
            ));
        }
        let root_policy = DocumentRootPolicy::create(
            config.roots,
            config.max_document_bytes,
            file_system.as_ref(),
        )
        .await?;
        let source = Self {
            descriptor: KnowledgeSourceDescriptor {
                id: config.source_id,
                name: config.name,
                description: config.description,
                domains: config.domains,
                capabilities: BTreeSet::from([
                    KnowledgeCapability::Search,
                    KnowledgeCapability::Read,
                    KnowledgeCapability::Revisioned,
                    KnowledgeCapability::SectionRead,
                    KnowledgeCapability::LineRead,
                ]),
                freshness: FreshnessClass::Periodic,
                filter_fields: vec![],
            },
            file_system,
            parsers,
            access_control,
            root_policy,
            source_secret: config.source_secret,
            catalog: RwLock::new(Arc::new(CatalogSnapshot::default())),
        };
        source.refresh().await?;
        Ok(source)
    }

    pub async fn refresh(&self) -> Result<DocumentRefreshReport, LocalDocumentSourceError> {
        let mut entries = BTreeMap::new();
        let mut skipped = 0;
        for root in &self.root_policy.roots {
            for candidate in self.file_system.list_files(&root.path).await? {
                let Some(extension) = candidate.extension().and_then(|value| value.to_str()) else {
                    skipped += 1;
                    continue;
                };
                if !self.parsers.supports(extension) {
                    skipped += 1;
                    continue;
                }
                let (root_id, canonical, relative) = match self
                    .root_policy
                    .validate(&candidate, self.file_system.as_ref())
                    .await
                {
                    Ok(value) => value,
                    Err(
                        LocalDocumentSourceError::RootViolation
                        | LocalDocumentSourceError::UnsupportedFile
                        | LocalDocumentSourceError::TooLarge,
                    ) => {
                        skipped += 1;
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let bytes = self.file_system.read(&canonical).await?;
                if bytes.len() > self.root_policy.max_document_bytes {
                    skipped += 1;
                    continue;
                }
                let parsed = match self.parsers.parse(&canonical, &bytes) {
                    Ok(parsed) => parsed,
                    Err(_) => {
                        skipped += 1;
                        continue;
                    }
                };
                let item_id = self.item_id(&root_id, &relative);
                entries.insert(
                    item_id.clone(),
                    CatalogEntry {
                        item_id,
                        canonical_path: canonical,
                        revision: content_revision(&bytes),
                        parsed,
                    },
                );
            }
        }
        let index = Bm25DocumentSearchIndex::build(
            entries
                .values()
                .map(|entry| IndexedDocument {
                    item_id: entry.item_id.clone(),
                    parsed: entry.parsed.clone(),
                })
                .collect(),
        );
        let report = DocumentRefreshReport {
            indexed: entries.len(),
            skipped,
        };
        *self
            .catalog
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Arc::new(CatalogSnapshot { entries, index });
        Ok(report)
    }

    fn snapshot(&self) -> Arc<CatalogSnapshot> {
        self.catalog
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn item_id(&self, root_id: &str, relative: &Path) -> String {
        let mut mac =
            HmacSha256::new_from_slice(&self.source_secret).expect("HMAC accepts any key length");
        mac.update(self.descriptor.id.as_bytes());
        mac.update(&[0]);
        mac.update(root_id.as_bytes());
        mac.update(&[0]);
        mac.update(relative.as_os_str().as_encoded_bytes());
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    }

    fn reference(&self, entry: &CatalogEntry) -> KnowledgeRef {
        KnowledgeRef {
            source_id: self.descriptor.id.clone(),
            item_id: entry.item_id.clone(),
            revision: Some(entry.revision.clone()),
        }
    }

    fn uri(&self, item_id: &str) -> String {
        format!("knowledge://{}/{}", self.descriptor.id, item_id)
    }

    fn backend_error(error: impl std::fmt::Display) -> KnowledgeError {
        KnowledgeError::Backend(error.to_string())
    }

    fn file_system_read_error(error: DocumentFileSystemError) -> KnowledgeError {
        match &error {
            DocumentFileSystemError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
                KnowledgeError::NotFound
            }
            _ => Self::backend_error(error),
        }
    }

    fn local_read_error(error: LocalDocumentSourceError) -> KnowledgeError {
        match error {
            LocalDocumentSourceError::RootViolation | LocalDocumentSourceError::UnsupportedFile => {
                KnowledgeError::Unauthorized
            }
            LocalDocumentSourceError::FileSystem(error) => Self::file_system_read_error(error),
            other => Self::backend_error(other),
        }
    }
}

impl KnowledgeSource for LocalDocumentSource {
    fn descriptor(&self) -> &KnowledgeSourceDescriptor {
        &self.descriptor
    }

    fn search<'a>(
        &'a self,
        ctx: KnowledgeRequestContext<'a>,
        request: SourceSearchRequest,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<SourceSearchPage, KnowledgeError>> {
        Box::pin(async move {
            if abort.is_cancelled() {
                return Err(KnowledgeError::Aborted);
            }
            self.access_control
                .authorize(
                    ctx.access,
                    KnowledgeAction::Search,
                    KnowledgeResourceRef::Source {
                        source_id: &self.descriptor.id,
                        domains: &self.descriptor.domains,
                    },
                )
                .await?;
            if !request.filters.is_empty() {
                return Err(KnowledgeError::InvalidFilter(
                    "local document source declares no filters".into(),
                ));
            }
            if request.cursor.is_some() {
                return Err(KnowledgeError::InvalidCursor);
            }
            if abort.is_cancelled() {
                return Err(KnowledgeError::Aborted);
            }

            let snapshot = self.snapshot();
            let hits = snapshot
                .index
                .search(&request.query, request.limit)
                .into_iter()
                .filter_map(|found| {
                    let entry = snapshot.entries.get(&found.item_id)?;
                    let sections = found
                        .section_ids
                        .into_iter()
                        .filter_map(|id| {
                            entry
                                .parsed
                                .sections
                                .iter()
                                .find(|section| section.id == id)
                                .map(|section| SectionRef {
                                    id,
                                    title: section.title.clone(),
                                })
                        })
                        .collect::<Vec<_>>();
                    Some(KnowledgeHit {
                        reference: self.reference(entry),
                        title: entry.parsed.title.clone(),
                        snippet: entry.parsed.text.chars().take(240).collect(),
                        suggested_selectors: if sections.is_empty() {
                            vec![ContentSelector::Document]
                        } else {
                            vec![ContentSelector::Sections { sections }]
                        },
                        uri: Some(self.uri(&entry.item_id)),
                        score: Some(found.score),
                        updated_at: None,
                        metadata: BTreeMap::new(),
                    })
                })
                .collect();
            Ok(SourceSearchPage {
                hits,
                next_cursor: None,
            })
        })
    }

    fn read<'a>(
        &'a self,
        ctx: KnowledgeRequestContext<'a>,
        request: KnowledgeReadRequest,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<KnowledgeContent, KnowledgeError>> {
        Box::pin(async move {
            if abort.is_cancelled() {
                return Err(KnowledgeError::Aborted);
            }
            if request.reference.source_id != self.descriptor.id {
                return Err(KnowledgeError::NotFound);
            }
            self.access_control
                .authorize(
                    ctx.access,
                    KnowledgeAction::Read,
                    KnowledgeResourceRef::Item(&request.reference),
                )
                .await?;
            let snapshot = self.snapshot();
            let entry = snapshot
                .entries
                .get(&request.reference.item_id)
                .ok_or(KnowledgeError::NotFound)?;
            let (_, canonical, _) = self
                .root_policy
                .validate(&entry.canonical_path, self.file_system.as_ref())
                .await
                .map_err(Self::local_read_error)?;
            if abort.is_cancelled() {
                return Err(KnowledgeError::Aborted);
            }
            let bytes = self
                .file_system
                .read(&canonical)
                .await
                .map_err(Self::file_system_read_error)?;
            if bytes.len() > self.root_policy.max_document_bytes {
                return Err(Self::backend_error("document exceeds size limit"));
            }
            let current_revision = content_revision(&bytes);
            if request.reference.revision.as_deref() != Some(&current_revision) {
                return Err(KnowledgeError::StaleReference {
                    latest: Some(KnowledgeRef {
                        source_id: self.descriptor.id.clone(),
                        item_id: entry.item_id.clone(),
                        revision: Some(current_revision),
                    }),
                });
            }
            let parsed = self
                .parsers
                .parse(&canonical, &bytes)
                .map_err(Self::backend_error)?;
            let selected = select_content(&parsed, &request.selector)?;
            let (text, truncated) = truncate_utf8(&selected, request.max_bytes);
            Ok(KnowledgeContent {
                reference: request.reference,
                selector: request.selector,
                title: parsed.title,
                blocks: vec![DataBlock::Text {
                    text,
                    mime_type: Some(parsed.mime_type),
                }],
                uri: Some(self.uri(&entry.item_id)),
                updated_at: None,
                obtained_at: Utc::now(),
                truncated,
                metadata: BTreeMap::new(),
            })
        })
    }
}

fn select_content(
    parsed: &ParsedDocument,
    selector: &ContentSelector,
) -> Result<String, KnowledgeError> {
    match selector {
        ContentSelector::Document => Ok(parsed.text.clone()),
        ContentSelector::Sections { sections } => {
            let mut selected = Vec::with_capacity(sections.len());
            for requested in sections {
                let section = parsed
                    .sections
                    .iter()
                    .find(|section| section.id == requested.id)
                    .ok_or(KnowledgeError::NotFound)?;
                selected.push(match &section.title {
                    Some(title) => format!("# {title}\n{}", section.text),
                    None => section.text.clone(),
                });
            }
            Ok(selected.join("\n\n"))
        }
        ContentSelector::LineRange { start, end } => {
            if *start == 0 || end < start {
                return Err(KnowledgeError::UnsupportedCapability(
                    "invalid logical line range".into(),
                ));
            }
            let lines = parsed.text.lines().collect::<Vec<_>>();
            let start_index = (*start as usize).saturating_sub(1);
            let end_index = (*end as usize).min(lines.len());
            if start_index >= lines.len() {
                return Err(KnowledgeError::NotFound);
            }
            Ok(lines[start_index..end_index].join("\n"))
        }
        ContentSelector::Chunks { .. } => Err(KnowledgeError::UnsupportedCapability(
            "local documents do not expose chunk selectors".into(),
        )),
    }
}

fn truncate_utf8(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut boundary = max_bytes.min(text.len());
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    (text[..boundary].to_string(), true)
}

fn is_safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use futures::future::BoxFuture;
    use llm_harness_runtime_knowledge::contract::{SourceContractCase, verify_source_contract};
    use llm_harness_runtime_knowledge::{
        AllowAllAuthorizer, AuthorizationDecision, KnowledgeAuthorizer, KnowledgeErrorCode,
        PrincipalRef,
    };
    use llm_harness_types::{RunContext, RunRequest};
    use tempfile::TempDir;

    use super::*;
    use crate::OsDocumentFileSystem;

    struct SubjectAuthorizer;

    impl KnowledgeAuthorizer for SubjectAuthorizer {
        fn authorize<'a>(
            &'a self,
            access: &'a llm_harness_runtime_knowledge::KnowledgeAccessContext,
            _action: KnowledgeAction,
            _resource: KnowledgeResourceRef<'a>,
        ) -> BoxFuture<'a, Result<AuthorizationDecision, KnowledgeError>> {
            Box::pin(async move {
                Ok(if access.principal.subject == "allowed" {
                    AuthorizationDecision::Allow
                } else {
                    AuthorizationDecision::Deny
                })
            })
        }
    }

    fn access(subject: &str) -> llm_harness_runtime_knowledge::KnowledgeAccessContext {
        llm_harness_runtime_knowledge::KnowledgeAccessContext::new(
            llm_harness_runtime_knowledge::KnowledgeScope::new("docs"),
            PrincipalRef::new(subject, "test"),
        )
    }

    fn config(root: &Path) -> LocalDocumentSourceConfig {
        LocalDocumentSourceConfig {
            source_id: "local-docs".into(),
            name: "Local docs".into(),
            description: "Test documents".into(),
            domains: vec!["policy".into()],
            roots: vec![DocumentRootConfig {
                id: "primary".into(),
                path: root.to_path_buf(),
            }],
            source_secret: vec![7; 32],
            max_document_bytes: 1024 * 1024,
        }
    }

    async fn source(
        root: &Path,
        access_control: Arc<KnowledgeAccessControl>,
    ) -> LocalDocumentSource {
        LocalDocumentSource::create(
            config(root),
            Arc::new(OsDocumentFileSystem),
            DocumentParserRegistry::default(),
            access_control,
        )
        .await
        .unwrap()
    }

    async fn source_with_roots(roots: Vec<DocumentRootConfig>) -> LocalDocumentSource {
        let mut source_config = config(&roots[0].path);
        source_config.roots = roots;
        LocalDocumentSource::create(
            source_config,
            Arc::new(OsDocumentFileSystem),
            DocumentParserRegistry::default(),
            Arc::new(KnowledgeAccessControl::new(Arc::new(AllowAllAuthorizer))),
        )
        .await
        .unwrap()
    }

    fn run(subject: &str) -> RunContext {
        RunContext::new(RunRequest::from_text("refund").with_extension(access(subject)))
    }

    async fn search_one(source: &LocalDocumentSource, run: &RunContext) -> KnowledgeHit {
        source
            .search(
                KnowledgeRequestContext::from_run(run).unwrap(),
                SourceSearchRequest {
                    query: "refund window".into(),
                    filters: vec![],
                    limit: 10,
                    cursor: None,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap()
            .hits
            .into_iter()
            .next()
            .unwrap()
    }

    #[tokio::test]
    async fn searches_and_reads_markdown_sections_without_exposing_paths() {
        let directory = TempDir::new().unwrap();
        let document_path = directory.path().join("refund-policy.md");
        fs::write(
            &document_path,
            "---\ntitle: Refund Policy\n---\n# Eligibility\nReceipt required.\n## Window\nRefund within thirty days.",
        )
        .unwrap();
        let source = source(
            directory.path(),
            Arc::new(KnowledgeAccessControl::new(Arc::new(AllowAllAuthorizer))),
        )
        .await;
        let run = run("allowed");
        let hit = search_one(&source, &run).await;
        assert!(!hit.reference.item_id.contains("refund"));
        let uri = hit.uri.as_deref().unwrap();
        assert!(uri.starts_with("knowledge://local-docs/"));
        assert!(!uri.contains(directory.path().to_string_lossy().as_ref()));

        let content = source
            .read(
                KnowledgeRequestContext::from_run(&run).unwrap(),
                KnowledgeReadRequest {
                    reference: hit.reference,
                    selector: ContentSelector::Sections {
                        sections: vec![SectionRef {
                            id: "eligibility/window".into(),
                            title: None,
                        }],
                    },
                    max_bytes: 4096,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let DataBlock::Text { text, mime_type } = &content.blocks[0] else {
            panic!("expected text content");
        };
        assert!(text.contains("thirty days"));
        assert_eq!(mime_type.as_deref(), Some("text/markdown"));
    }

    #[tokio::test]
    async fn exact_revision_never_silently_upgrades_to_latest() {
        let directory = TempDir::new().unwrap();
        let document_path = directory.path().join("policy.txt");
        fs::write(&document_path, "old refund policy").unwrap();
        let source = source(
            directory.path(),
            Arc::new(KnowledgeAccessControl::new(Arc::new(AllowAllAuthorizer))),
        )
        .await;
        let run = run("allowed");
        let hit = source
            .search(
                KnowledgeRequestContext::from_run(&run).unwrap(),
                SourceSearchRequest {
                    query: "refund".into(),
                    filters: vec![],
                    limit: 1,
                    cursor: None,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap()
            .hits
            .remove(0);
        fs::write(&document_path, "new refund policy").unwrap();

        let error = source
            .read(
                KnowledgeRequestContext::from_run(&run).unwrap(),
                KnowledgeReadRequest {
                    reference: hit.reference,
                    selector: ContentSelector::Document,
                    max_bytes: 4096,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), KnowledgeErrorCode::StaleReference);
    }

    #[tokio::test]
    async fn deleted_document_returns_not_found() {
        let directory = TempDir::new().unwrap();
        let document_path = directory.path().join("policy.txt");
        fs::write(&document_path, "refund policy").unwrap();
        let source = source(
            directory.path(),
            Arc::new(KnowledgeAccessControl::new(Arc::new(AllowAllAuthorizer))),
        )
        .await;
        let run = run("allowed");
        let reference = source
            .search(
                KnowledgeRequestContext::from_run(&run).unwrap(),
                SourceSearchRequest {
                    query: "refund".into(),
                    filters: vec![],
                    limit: 1,
                    cursor: None,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap()
            .hits
            .remove(0)
            .reference;
        fs::remove_file(document_path).unwrap();
        let error = source
            .read(
                KnowledgeRequestContext::from_run(&run).unwrap(),
                KnowledgeReadRequest {
                    reference,
                    selector: ContentSelector::Document,
                    max_bytes: 4096,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), KnowledgeErrorCode::NotFound);
    }

    #[tokio::test]
    async fn stable_root_ids_make_references_independent_of_root_order() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        fs::write(first.path().join("policy.txt"), "alpha-root-marker").unwrap();
        fs::write(second.path().join("policy.txt"), "beta-root-marker").unwrap();
        let roots = [
            DocumentRootConfig {
                id: "alpha".into(),
                path: first.path().to_path_buf(),
            },
            DocumentRootConfig {
                id: "beta".into(),
                path: second.path().to_path_buf(),
            },
        ];
        let source_ab = source_with_roots(roots.to_vec()).await;
        let source_ba = source_with_roots(roots.into_iter().rev().collect()).await;
        let run = run("allowed");
        let request = || SourceSearchRequest {
            query: "alpha-root-marker".into(),
            filters: vec![],
            limit: 1,
            cursor: None,
        };
        let reference_ab = source_ab
            .search(
                KnowledgeRequestContext::from_run(&run).unwrap(),
                request(),
                CancellationToken::new(),
            )
            .await
            .unwrap()
            .hits
            .remove(0)
            .reference;
        let reference_ba = source_ba
            .search(
                KnowledgeRequestContext::from_run(&run).unwrap(),
                request(),
                CancellationToken::new(),
            )
            .await
            .unwrap()
            .hits
            .remove(0)
            .reference;
        assert_eq!(reference_ab, reference_ba);
    }

    #[tokio::test]
    async fn local_source_passes_shared_source_contract() {
        let directory = TempDir::new().unwrap();
        fs::write(
            directory.path().join("refund.md"),
            "# Refund\nRefund requests have a thirty day window.",
        )
        .unwrap();
        let source = source(
            directory.path(),
            Arc::new(KnowledgeAccessControl::new(Arc::new(SubjectAuthorizer))),
        )
        .await;
        let run = run("allowed");
        let reference = search_one(&source, &run).await.reference;
        let case = SourceContractCase {
            allowed_access: access("allowed"),
            denied_access: access("denied"),
            search: SourceSearchRequest {
                query: "refund".into(),
                filters: vec![],
                limit: 10,
                cursor: None,
            },
            expected_reference: reference.clone(),
            selector: ContentSelector::Document,
            max_bytes: 4096,
            missing_reference: KnowledgeRef {
                source_id: "local-docs".into(),
                item_id: "missing".into(),
                revision: Some("missing".into()),
            },
            stale_reference: Some(KnowledgeRef {
                revision: Some("old-revision".into()),
                ..reference
            }),
        };
        verify_source_contract(&source, &run, &case).await.unwrap();
    }

    #[tokio::test]
    async fn parent_components_are_rejected_before_canonicalization() {
        let directory = TempDir::new().unwrap();
        let outside = directory.path().parent().unwrap().join("outside.txt");
        fs::write(&outside, "secret").unwrap();
        let policy = DocumentRootPolicy::create(
            vec![DocumentRootConfig {
                id: "primary".into(),
                path: directory.path().to_path_buf(),
            }],
            1024,
            &OsDocumentFileSystem,
        )
        .await
        .unwrap();
        let candidate = directory.path().join("..").join("outside.txt");
        let error = policy
            .validate(&candidate, &OsDocumentFileSystem)
            .await
            .unwrap_err();
        assert!(matches!(error, LocalDocumentSourceError::RootViolation));
        let _ = fs::remove_file(outside);
    }

    #[tokio::test]
    async fn catalog_never_indexes_a_symlink_target_outside_root() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(root.path().join("safe.txt"), "safe local content").unwrap();
        let outside_file = outside.path().join("outside.txt");
        fs::write(&outside_file, "external-secret-marker").unwrap();
        let link = root.path().join("linked.txt");
        if create_file_symlink(&outside_file, &link).is_err() {
            return;
        }

        let source = source(
            root.path(),
            Arc::new(KnowledgeAccessControl::new(Arc::new(AllowAllAuthorizer))),
        )
        .await;
        let results = source
            .search(
                KnowledgeRequestContext::from_run(&run("allowed")).unwrap(),
                SourceSearchRequest {
                    query: "external-secret-marker".into(),
                    filters: vec![],
                    limit: 10,
                    cursor: None,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(results.hits.is_empty());
    }

    #[test]
    fn truncation_preserves_utf8_boundaries() {
        let (text, truncated) = truncate_utf8("退款政策", 5);
        assert_eq!(text, "退");
        assert!(truncated);
    }

    #[test]
    fn source_ids_cannot_change_the_knowledge_uri_structure() {
        assert!(is_safe_identifier("local.docs-1"));
        assert!(!is_safe_identifier("local/docs"));
        assert!(!is_safe_identifier("local docs"));
        assert!(!is_safe_identifier(""));
    }

    #[cfg(unix)]
    fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_file(target, link)
    }
}
