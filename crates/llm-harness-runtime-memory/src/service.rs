use std::sync::Arc;

use chrono::Utc;
use llm_harness_runtime_knowledge::{
    KnowledgeAccessControl, KnowledgeAction, KnowledgeCapability, KnowledgeError, KnowledgeRef,
    KnowledgeRequestContext, KnowledgeResourceRef, KnowledgeSource,
};
use tokio_util::sync::CancellationToken;

use crate::{
    MemoryConsistency, MemoryDeleteReceipt, MemoryMutation, MemoryMutationGate,
    MemoryMutationGateError, MemoryMutationOrigin, MemoryMutationRequest, MemoryPolicyError,
    MemoryProvenance, MemorySessionId, MemoryStore, MemoryVisibility, MemoryWrite,
    MemoryWriteIntent, MemoryWritePolicy, MemoryWriteReceipt,
};

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum MemoryServiceBuildError {
    #[error("memory read source ID must not be empty")]
    EmptyReadSourceId,
    #[error("memory store read source ID `{store_source_id}` does not match `{read_source_id}`")]
    ReadSourceMismatch {
        read_source_id: String,
        store_source_id: String,
    },
    #[error("memory read source must advertise capability {0:?}")]
    MissingReadCapability(KnowledgeCapability),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryWriteViolation {
    EmptyContent,
    EmptyKind,
    EmptyIdempotencyKey,
    ProvenanceMismatch,
    InvalidExpiration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryReceiptViolation {
    WrongSource,
    EmptyItemId,
    MissingRevision,
    ConsistencyMismatch,
    EmptyCheckpoint,
    WrongDeletedItem,
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryServiceError {
    #[error(transparent)]
    Knowledge(#[from] KnowledgeError),
    #[error(transparent)]
    Policy(#[from] MemoryPolicyError),
    #[error(transparent)]
    MutationGate(#[from] MemoryMutationGateError),
    #[error("memory mutation origin is invalid")]
    InvalidOrigin,
    #[error("memory write policy returned an invalid record")]
    InvalidWrite(MemoryWriteViolation),
    #[error("memory store returned an invalid receipt")]
    InvalidReceipt(MemoryReceiptViolation),
}

pub struct MemoryService {
    access_control: Arc<KnowledgeAccessControl>,
    read_source: Arc<dyn KnowledgeSource>,
    write_store: Arc<dyn MemoryStore>,
    write_policy: Arc<dyn MemoryWritePolicy>,
    mutation_gate: Arc<dyn MemoryMutationGate>,
}

impl MemoryService {
    pub fn new(
        access_control: Arc<KnowledgeAccessControl>,
        read_source: Arc<dyn KnowledgeSource>,
        write_store: Arc<dyn MemoryStore>,
        write_policy: Arc<dyn MemoryWritePolicy>,
        mutation_gate: Arc<dyn MemoryMutationGate>,
    ) -> Result<Self, MemoryServiceBuildError> {
        let read_descriptor = read_source.descriptor();
        if read_descriptor.id.trim().is_empty() {
            return Err(MemoryServiceBuildError::EmptyReadSourceId);
        }
        let store_source_id = &write_store.descriptor().read_source_id;
        if store_source_id != &read_descriptor.id {
            return Err(MemoryServiceBuildError::ReadSourceMismatch {
                read_source_id: read_descriptor.id.clone(),
                store_source_id: store_source_id.clone(),
            });
        }
        for capability in [
            KnowledgeCapability::Search,
            KnowledgeCapability::Read,
            KnowledgeCapability::Revisioned,
        ] {
            if !read_descriptor.capabilities.contains(&capability) {
                return Err(MemoryServiceBuildError::MissingReadCapability(capability));
            }
        }
        Ok(Self {
            access_control,
            read_source,
            write_store,
            write_policy,
            mutation_gate,
        })
    }

    pub fn read_source(&self) -> Arc<dyn KnowledgeSource> {
        self.read_source.clone()
    }

    pub fn store_descriptor(&self) -> &crate::MemoryStoreDescriptor {
        self.write_store.descriptor()
    }

    pub async fn write(
        &self,
        ctx: KnowledgeRequestContext<'_>,
        intent: MemoryWriteIntent,
        origin: MemoryMutationOrigin,
        abort: CancellationToken,
    ) -> Result<MemoryWriteReceipt, MemoryServiceError> {
        abort_if_cancelled(&abort)?;
        let descriptor = self.read_source.descriptor();
        self.access_control
            .authorize(
                ctx.access,
                KnowledgeAction::Write,
                KnowledgeResourceRef::Source {
                    source_id: &descriptor.id,
                    domains: &descriptor.domains,
                },
            )
            .await?;
        abort_if_cancelled(&abort)?;
        if !valid_origin(&origin) {
            return Err(MemoryServiceError::InvalidOrigin);
        }
        let provenance = MemoryProvenance {
            run_id: ctx.run.id(),
            session_id: ctx
                .run
                .extension::<MemorySessionId>()
                .map(|session| session.as_str().to_string()),
            origin: origin.clone(),
            recorded_at: Utc::now(),
        };
        let write = self
            .write_policy
            .prepare(ctx, intent, provenance.clone(), abort.clone())
            .await?;
        abort_if_cancelled(&abort)?;
        validate_write(&write, &provenance)?;
        self.mutation_gate
            .authorize(
                ctx,
                MemoryMutationRequest {
                    mutation: MemoryMutation::Write {
                        write: write.clone(),
                    },
                    origin,
                },
                abort.clone(),
            )
            .await?;
        abort_if_cancelled(&abort)?;
        let receipt = self.write_store.upsert(ctx, write, abort.clone()).await?;
        abort_if_cancelled(&abort)?;
        validate_receipt(
            &receipt.reference,
            &receipt.visibility,
            self.write_store.descriptor(),
        )?;
        Ok(receipt)
    }

    pub async fn delete(
        &self,
        ctx: KnowledgeRequestContext<'_>,
        reference: KnowledgeRef,
        origin: MemoryMutationOrigin,
        abort: CancellationToken,
    ) -> Result<MemoryDeleteReceipt, MemoryServiceError> {
        abort_if_cancelled(&abort)?;
        if reference.source_id != self.read_source.descriptor().id {
            return Err(KnowledgeError::NotFound.into());
        }
        if !valid_origin(&origin) {
            return Err(MemoryServiceError::InvalidOrigin);
        }
        self.access_control
            .authorize(
                ctx.access,
                KnowledgeAction::Delete,
                KnowledgeResourceRef::Item(&reference),
            )
            .await?;
        abort_if_cancelled(&abort)?;
        self.mutation_gate
            .authorize(
                ctx,
                MemoryMutationRequest {
                    mutation: MemoryMutation::Delete {
                        reference: reference.clone(),
                    },
                    origin,
                },
                abort.clone(),
            )
            .await?;
        abort_if_cancelled(&abort)?;
        let receipt = self
            .write_store
            .delete(ctx, reference.clone(), abort.clone())
            .await?;
        abort_if_cancelled(&abort)?;
        validate_receipt(
            &receipt.reference,
            &receipt.visibility,
            self.write_store.descriptor(),
        )?;
        if receipt.reference.source_id != reference.source_id
            || receipt.reference.item_id != reference.item_id
        {
            return Err(MemoryServiceError::InvalidReceipt(
                MemoryReceiptViolation::WrongDeletedItem,
            ));
        }
        Ok(receipt)
    }
}

fn abort_if_cancelled(abort: &CancellationToken) -> Result<(), MemoryServiceError> {
    if abort.is_cancelled() {
        Err(KnowledgeError::Aborted.into())
    } else {
        Ok(())
    }
}

fn valid_origin(origin: &MemoryMutationOrigin) -> bool {
    match origin {
        MemoryMutationOrigin::ExplicitTool { tool_use_id } => !tool_use_id.trim().is_empty(),
        MemoryMutationOrigin::Application { operation_id } => !operation_id.trim().is_empty(),
        MemoryMutationOrigin::Capture {
            policy_id,
            candidate_id,
        } => !policy_id.trim().is_empty() && !candidate_id.trim().is_empty(),
    }
}

fn validate_write(
    write: &MemoryWrite,
    provenance: &MemoryProvenance,
) -> Result<(), MemoryServiceError> {
    let violation = if write.content.trim().is_empty() {
        Some(MemoryWriteViolation::EmptyContent)
    } else if write
        .kind
        .as_ref()
        .is_some_and(|kind| kind.trim().is_empty())
    {
        Some(MemoryWriteViolation::EmptyKind)
    } else if write.idempotency_key.trim().is_empty() {
        Some(MemoryWriteViolation::EmptyIdempotencyKey)
    } else if &write.provenance != provenance {
        Some(MemoryWriteViolation::ProvenanceMismatch)
    } else if write
        .expires_at
        .is_some_and(|expires_at| expires_at <= provenance.recorded_at)
    {
        Some(MemoryWriteViolation::InvalidExpiration)
    } else {
        None
    };
    match violation {
        Some(violation) => Err(MemoryServiceError::InvalidWrite(violation)),
        None => Ok(()),
    }
}

fn validate_receipt(
    reference: &KnowledgeRef,
    visibility: &MemoryVisibility,
    descriptor: &crate::MemoryStoreDescriptor,
) -> Result<(), MemoryServiceError> {
    let violation = if reference.source_id != descriptor.read_source_id {
        Some(MemoryReceiptViolation::WrongSource)
    } else if reference.item_id.trim().is_empty() {
        Some(MemoryReceiptViolation::EmptyItemId)
    } else if reference
        .revision
        .as_ref()
        .is_none_or(|revision| revision.trim().is_empty())
    {
        Some(MemoryReceiptViolation::MissingRevision)
    } else {
        match (descriptor.consistency, visibility) {
            (MemoryConsistency::Immediate, MemoryVisibility::Visible) => None,
            (MemoryConsistency::Eventual, MemoryVisibility::Pending { checkpoint })
                if checkpoint.trim().is_empty() =>
            {
                Some(MemoryReceiptViolation::EmptyCheckpoint)
            }
            (MemoryConsistency::Eventual, MemoryVisibility::Pending { .. }) => None,
            _ => Some(MemoryReceiptViolation::ConsistencyMismatch),
        }
    };
    match violation {
        Some(violation) => Err(MemoryServiceError::InvalidReceipt(violation)),
        None => Ok(()),
    }
}
