use std::{sync::Arc, sync::OnceLock, time::Duration};

use futures::future::BoxFuture;
use llm_harness_runtime_knowledge::{KnowledgeError, KnowledgeRef, KnowledgeRequestContext};
use llm_harness_types::{
    DataBlock, PersistedEnvelope, ProjectionMetadata, Tool, ToolContext, ToolFailure, ToolResult,
    ToolResultExtensions, ToolSessionProjection,
};

use crate::{
    MemoryMutationGateError, MemoryMutationOrigin, MemoryPolicyError, MemoryPolicyRejection,
    MemoryService, MemoryServiceError, MemoryWriteIntent,
};

pub const MEMORY_WRITE_TOOL_NAME: &str = "memory_write";
pub const MEMORY_FORGET_TOOL_NAME: &str = "memory_forget";

pub struct MemoryWriteTool {
    service: Arc<MemoryService>,
}

impl MemoryWriteTool {
    pub fn new(service: Arc<MemoryService>) -> Self {
        Self { service }
    }
}

impl Tool for MemoryWriteTool {
    fn name(&self) -> &str {
        MEMORY_WRITE_TOOL_NAME
    }

    fn label(&self) -> &str {
        "Remember information"
    }

    fn description(&self) -> &str {
        "Persist a durable user-approved fact or preference. Do not use for secrets, external documents, or unapproved inferences."
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "The concise durable fact or preference the user approved remembering."
                    },
                    "kind": {
                        "type": "string",
                        "description": "Optional application-defined memory category."
                    },
                    "ttl_seconds": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional requested lifetime; policy determines the final TTL."
                    }
                },
                "required": ["content"],
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
            let content = args
                .get("content")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| ToolFailure::invalid_arguments("content must be a string"))?;
            let kind = args
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let requested_ttl = match args.get("ttl_seconds") {
                Some(value) => Some(Duration::from_secs(value.as_u64().ok_or_else(|| {
                    ToolFailure::invalid_arguments("ttl_seconds must be a positive integer")
                })?)),
                None => None,
            };
            let request_context =
                KnowledgeRequestContext::from_run(&ctx.run).map_err(memory_knowledge_failure)?;
            let receipt = self
                .service
                .write(
                    request_context,
                    MemoryWriteIntent {
                        content: content.into(),
                        kind,
                        requested_ttl,
                    },
                    MemoryMutationOrigin::ExplicitTool {
                        tool_use_id: ctx.tool_use_id.clone(),
                    },
                    ctx.abort.clone(),
                )
                .await
                .map_err(memory_service_failure)?;
            let value =
                serde_json::to_value(&receipt).expect("memory write receipt must be serializable");
            let summary = format!(
                "Memory stored as {} at revision {:?}.",
                receipt.reference.item_id, receipt.reference.revision
            );
            Ok(projected_result("memory.write", summary, value))
        })
    }
}

pub struct MemoryForgetTool {
    service: Arc<MemoryService>,
}

impl MemoryForgetTool {
    pub fn new(service: Arc<MemoryService>) -> Self {
        Self { service }
    }
}

impl Tool for MemoryForgetTool {
    fn name(&self) -> &str {
        MEMORY_FORGET_TOOL_NAME
    }

    fn label(&self) -> &str {
        "Forget information"
    }

    fn description(&self) -> &str {
        "Delete a memory using an exact reference previously returned by memory or knowledge tools."
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "reference": {
                        "type": "object",
                        "properties": {
                            "source_id": {"type": "string"},
                            "item_id": {"type": "string"},
                            "revision": {"type": "string"}
                        },
                        "required": ["source_id", "item_id", "revision"],
                        "additionalProperties": false
                    }
                },
                "required": ["reference"],
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
            let reference: KnowledgeRef = serde_json::from_value(
                args.get("reference")
                    .cloned()
                    .ok_or_else(|| ToolFailure::invalid_arguments("reference is required"))?,
            )
            .map_err(|_| ToolFailure::invalid_arguments("reference is invalid"))?;
            let request_context =
                KnowledgeRequestContext::from_run(&ctx.run).map_err(memory_knowledge_failure)?;
            let receipt = self
                .service
                .delete(
                    request_context,
                    reference,
                    MemoryMutationOrigin::ExplicitTool {
                        tool_use_id: ctx.tool_use_id.clone(),
                    },
                    ctx.abort.clone(),
                )
                .await
                .map_err(memory_service_failure)?;
            let value =
                serde_json::to_value(&receipt).expect("memory delete receipt must be serializable");
            let summary = format!(
                "Memory {} deletion accepted at revision {:?}.",
                receipt.reference.item_id, receipt.reference.revision
            );
            Ok(projected_result("memory.delete", summary, value))
        })
    }
}

fn projected_result(namespace: &str, summary: String, value: serde_json::Value) -> ToolResult {
    let content = vec![DataBlock::text(summary)];
    ToolResult {
        model_content: content.clone(),
        details: value.clone(),
        extensions: ToolResultExtensions::new(),
        terminate: false,
        session_projection: ToolSessionProjection::Projected {
            content,
            metadata: ProjectionMetadata {
                envelopes: vec![PersistedEnvelope {
                    namespace: namespace.into(),
                    schema_version: 1,
                    value,
                }],
            },
        },
    }
}

fn memory_service_failure(error: MemoryServiceError) -> ToolFailure {
    match error {
        MemoryServiceError::Knowledge(error) => memory_knowledge_failure(error),
        MemoryServiceError::Policy(error) => memory_policy_failure(error),
        MemoryServiceError::MutationGate(error) => memory_mutation_gate_failure(error),
        MemoryServiceError::InvalidOrigin
        | MemoryServiceError::InvalidWrite(_)
        | MemoryServiceError::InvalidReceipt(_) => ToolFailure::new(
            "memory_protocol_failed",
            "Memory could not be updated safely.",
        ),
    }
}

fn memory_mutation_gate_failure(error: MemoryMutationGateError) -> ToolFailure {
    match error {
        MemoryMutationGateError::Denied => ToolFailure::new(
            "memory_approval_denied",
            "Memory mutation was not approved.",
        ),
        MemoryMutationGateError::Unavailable => ToolFailure::new(
            "memory_approval_unavailable",
            "Memory mutation approval is unavailable.",
        ),
        MemoryMutationGateError::Aborted => {
            ToolFailure::new("memory_aborted", "Memory operation was aborted.")
        }
        MemoryMutationGateError::Backend(_) => ToolFailure::new(
            "memory_approval_failed",
            "Memory mutation could not be approved safely.",
        ),
    }
}

fn memory_knowledge_failure(error: KnowledgeError) -> ToolFailure {
    let mut failure = ToolFailure::new(
        format!("memory_{}", error.code().as_str()),
        error.to_string(),
    );
    failure.retryable = error.retryable();
    failure
}

fn memory_policy_failure(error: MemoryPolicyError) -> ToolFailure {
    match error {
        MemoryPolicyError::Rejected(rejection) => ToolFailure::new(
            format!("memory_{}", rejection.as_str()),
            rejection_message(rejection),
        ),
        MemoryPolicyError::Aborted => {
            ToolFailure::new("memory_aborted", "Memory operation was aborted.")
        }
        MemoryPolicyError::Backend(_) => ToolFailure::new(
            "memory_policy_failed",
            "Memory could not be evaluated safely.",
        ),
    }
}

fn rejection_message(rejection: MemoryPolicyRejection) -> &'static str {
    match rejection {
        MemoryPolicyRejection::EmptyContent => "Memory content must not be empty.",
        MemoryPolicyRejection::ContentTooLarge => "Memory content is too large.",
        MemoryPolicyRejection::SensitiveContent => {
            "Sensitive credentials or secrets cannot be stored as memory."
        }
        MemoryPolicyRejection::UnsupportedKind => "Memory kind is not allowed.",
        MemoryPolicyRejection::InvalidTtl => "Requested memory lifetime is not allowed.",
        MemoryPolicyRejection::Other => "Memory write was rejected by policy.",
    }
}
