use std::sync::{Arc, OnceLock};

use futures::future::BoxFuture;
use llm_harness_types::{
    DataBlock, PersistedEnvelope, ProjectionMetadata, Tool, ToolContext, ToolFailure, ToolResult,
    ToolResultExtensions, ToolSessionProjection,
};

use crate::{
    CitationRecord, ContentSelector, EvidenceAuthority, EvidenceProviderId, KnowledgeAccessContext,
    KnowledgeError, KnowledgeEvidenceSet, KnowledgeReadRequest, KnowledgeRef, KnowledgeRegistry,
    KnowledgeRunState, KnowledgeSearchRequest,
};

pub const KNOWLEDGE_SEARCH_TOOL_NAME: &str = "knowledge_search";
pub const KNOWLEDGE_READ_TOOL_NAME: &str = "knowledge_read";

#[derive(Debug, Clone)]
pub struct KnowledgeToolConfig {
    pub max_search_results: usize,
    pub max_read_bytes: usize,
}

impl Default for KnowledgeToolConfig {
    fn default() -> Self {
        Self {
            max_search_results: 10,
            max_read_bytes: 32 * 1024,
        }
    }
}

pub struct KnowledgeSearchTool {
    registry: Arc<KnowledgeRegistry>,
    config: KnowledgeToolConfig,
}

impl KnowledgeSearchTool {
    pub fn new(registry: Arc<KnowledgeRegistry>, config: KnowledgeToolConfig) -> Self {
        Self { registry, config }
    }
}

impl Tool for KnowledgeSearchTool {
    fn name(&self) -> &str {
        KNOWLEDGE_SEARCH_TOOL_NAME
    }

    fn label(&self) -> &str {
        "Search knowledge"
    }

    fn description(&self) -> &str {
        "Search an authorized knowledge source. Returns lightweight references; use knowledge_read to inspect evidence."
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "source_id": {"type": "string"},
                    "domains": {"type": "array", "items": {"type": "string"}},
                    "limit": {"type": "integer", "minimum": 1}
                },
                "required": ["query"],
                "additionalProperties": false
            })
        })
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let query = args
                .get("query")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    ToolFailure::invalid_arguments("query must be a non-empty string")
                })?;
            let access = ctx
                .run
                .extension::<KnowledgeAccessContext>()
                .ok_or_else(|| knowledge_failure(KnowledgeError::Unauthorized))?;
            let source_ids = args
                .get("source_id")
                .and_then(serde_json::Value::as_str)
                .map(|value| vec![value.to_string()])
                .unwrap_or_default();
            let domains = args
                .get("domains")
                .and_then(serde_json::Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let limit = args
                .get("limit")
                .and_then(serde_json::Value::as_u64)
                .map(|value| value as usize)
                .unwrap_or(self.config.max_search_results)
                .min(self.config.max_search_results);
            let page = self
                .registry
                .search(
                    &ctx.run,
                    access,
                    KnowledgeSearchRequest {
                        query: query.into(),
                        domains,
                        source_ids,
                        filters: vec![],
                        limit,
                        cursor: None,
                    },
                    ctx.abort.clone(),
                )
                .await
                .map_err(knowledge_failure)?;
            let public_hits = page
                .hits
                .iter()
                .map(|hit| {
                    serde_json::json!({
                        "reference": hit.reference,
                        "title": hit.title,
                        "snippet": hit.snippet,
                        "suggested_selectors": hit.suggested_selectors,
                        "uri": hit.uri,
                    })
                })
                .collect::<Vec<_>>();
            let text = serde_json::to_string_pretty(&serde_json::json!({"hits": public_hits}))
                .expect("knowledge hits are serializable");
            let content = vec![DataBlock::text(text)];
            let metadata = ProjectionMetadata {
                envelopes: vec![PersistedEnvelope {
                    namespace: "knowledge.search".into(),
                    schema_version: 1,
                    value: serde_json::json!({"hits": page.hits.iter().map(|hit| &hit.reference).collect::<Vec<_>>() }),
                }],
            };
            Ok(ToolResult {
                model_content: content.clone(),
                details: serde_json::json!({"hits": public_hits}),
                extensions: ToolResultExtensions::new(),
                terminate: false,
                session_projection: ToolSessionProjection::Projected { content, metadata },
            })
        })
    }
}

pub struct KnowledgeReadTool {
    registry: Arc<KnowledgeRegistry>,
    authority: Arc<EvidenceAuthority>,
    provider_id: EvidenceProviderId,
    config: KnowledgeToolConfig,
}

impl KnowledgeReadTool {
    pub fn new(
        registry: Arc<KnowledgeRegistry>,
        authority: Arc<EvidenceAuthority>,
        provider_id: EvidenceProviderId,
        config: KnowledgeToolConfig,
    ) -> Self {
        Self {
            registry,
            authority,
            provider_id,
            config,
        }
    }
}

impl Tool for KnowledgeReadTool {
    fn name(&self) -> &str {
        KNOWLEDGE_READ_TOOL_NAME
    }

    fn label(&self) -> &str {
        "Read knowledge"
    }

    fn description(&self) -> &str {
        "Read authorized evidence using an opaque reference returned by knowledge_search."
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();
        SCHEMA.get_or_init(|| serde_json::json!({
            "type": "object",
            "properties": {
                "reference": {
                    "type": "object",
                    "properties": {
                        "source_id": {"type": "string"},
                        "item_id": {"type": "string"},
                        "revision": {"type": ["string", "null"]}
                    },
                    "required": ["source_id", "item_id"],
                    "additionalProperties": false
                },
                "selector": {
                    "description": "Structured selector returned or suggested by knowledge_search.",
                    "oneOf": [
                        {
                            "type": "object",
                            "properties": {
                                "kind": {"const": "document"}
                            },
                            "required": ["kind"],
                            "additionalProperties": false
                        },
                        {
                            "type": "object",
                            "properties": {
                                "kind": {"const": "sections"},
                                "sections": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "id": {"type": "string"},
                                            "title": {"type": ["string", "null"]}
                                        },
                                        "required": ["id"],
                                        "additionalProperties": false
                                    }
                                }
                            },
                            "required": ["kind", "sections"],
                            "additionalProperties": false
                        },
                        {
                            "type": "object",
                            "properties": {
                                "kind": {"const": "line_range"},
                                "start": {"type": "integer", "minimum": 1},
                                "end": {"type": "integer", "minimum": 1}
                            },
                            "required": ["kind", "start", "end"],
                            "additionalProperties": false
                        },
                        {
                            "type": "object",
                            "properties": {
                                "kind": {"const": "chunks"},
                                "ids": {
                                    "type": "array",
                                    "items": {"type": "string"}
                                }
                            },
                            "required": ["kind", "ids"],
                            "additionalProperties": false
                        }
                    ]
                }
            },
            "required": ["reference"],
            "additionalProperties": false
        }))
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let reference: KnowledgeRef = serde_json::from_value(
                args.get("reference")
                    .cloned()
                    .ok_or_else(|| ToolFailure::invalid_arguments("reference is required"))?,
            )
            .map_err(|_| ToolFailure::invalid_arguments("reference is invalid"))?;
            let selector = match args.get("selector") {
                Some(value) => serde_json::from_value::<ContentSelector>(value.clone())
                    .map_err(|_| ToolFailure::invalid_arguments("selector is invalid"))?,
                None => ContentSelector::Document,
            };
            let access = ctx
                .run
                .extension::<KnowledgeAccessContext>()
                .ok_or_else(|| knowledge_failure(KnowledgeError::Unauthorized))?;
            let content = self
                .registry
                .read(
                    &ctx.run,
                    access,
                    KnowledgeReadRequest {
                        reference,
                        selector,
                        max_bytes: self.config.max_read_bytes,
                    },
                    ctx.abort.clone(),
                )
                .await
                .map_err(knowledge_failure)?;
            let evidence = self
                .authority
                .issuer(&self.provider_id, &ctx.run, access)
                .map_err(|_| {
                    ToolFailure::new("evidence_issue_failed", "Evidence could not be verified.")
                })?
                .issue(
                    Some(&ctx.tool_use_id),
                    content.reference.clone(),
                    content.blocks.clone(),
                    content.obtained_at,
                )
                .map_err(|_| {
                    ToolFailure::new("evidence_issue_failed", "Evidence could not be verified.")
                })?;
            let persisted = self
                .authority
                .persisted(&evidence, &ctx.run, access, Some(&ctx.tool_use_id))
                .ok_or_else(|| {
                    ToolFailure::new(
                        "evidence_verification_failed",
                        "Evidence could not be verified.",
                    )
                })?;
            let state = ctx
                .run
                .local()
                .get_or_insert_with(KnowledgeRunState::default);
            let citation: CitationRecord = state.register(ctx.run.id(), &persisted);
            let mut model_content = vec![DataBlock::text(format!(
                "Citation handle: {}",
                citation.handle
            ))];
            model_content.extend(content.blocks.clone());
            let mut extensions = ToolResultExtensions::new();
            extensions.insert(KnowledgeEvidenceSet(vec![evidence]));
            Ok(ToolResult {
                model_content,
                details: serde_json::json!({
                    "citation": citation,
                    "uri": content.uri,
                    "truncated": content.truncated,
                }),
                extensions,
                terminate: false,
                session_projection: ToolSessionProjection::Ephemeral {
                    summary: format!(
                        "Read {} at revision {:?}; citation {}",
                        content.reference.item_id, content.reference.revision, citation.handle
                    ),
                    metadata: ProjectionMetadata {
                        envelopes: vec![PersistedEnvelope {
                            namespace: "knowledge.evidence".into(),
                            schema_version: 1,
                            value: serde_json::to_value(&persisted)
                                .expect("persisted evidence is serializable"),
                        }],
                    },
                },
            })
        })
    }
}

fn knowledge_failure(error: KnowledgeError) -> ToolFailure {
    let code = format!("knowledge_{}", error.code().as_str());
    let mut failure = ToolFailure::new(code, error.to_string());
    failure.retryable = error.retryable();
    failure.details = serde_json::json!({"code": error.code()});
    failure
}
