//! Reusable semantic contract checks for every [`MemoryStore`].

use llm_harness_runtime_knowledge::{
    KnowledgeAccessContext, KnowledgeError, KnowledgeErrorCode, KnowledgeRef,
    KnowledgeRequestContext,
};
use llm_harness_types::RunContext;
use tokio_util::sync::CancellationToken;

use crate::{MemoryConsistency, MemoryStore, MemoryVisibility, MemoryWrite};

#[derive(Clone)]
pub struct MemoryStoreContractCase {
    pub allowed_access: KnowledgeAccessContext,
    pub denied_access: KnowledgeAccessContext,
    pub write: MemoryWrite,
    pub expected_reference: KnowledgeRef,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("memory store contract check `{check}` failed: {message}")]
pub struct MemoryStoreContractViolation {
    pub check: &'static str,
    pub message: String,
}

fn violation(
    check: &'static str,
    message: impl Into<String>,
) -> Result<(), MemoryStoreContractViolation> {
    Err(MemoryStoreContractViolation {
        check,
        message: message.into(),
    })
}

fn expect_error_code(
    check: &'static str,
    result: Result<impl Sized, KnowledgeError>,
    expected: KnowledgeErrorCode,
) -> Result<(), MemoryStoreContractViolation> {
    match result {
        Err(error) if error.code() == expected => Ok(()),
        Err(error) => violation(
            check,
            format!("expected {expected:?}, received {:?}", error.code()),
        ),
        Ok(_) => violation(check, format!("expected {expected:?}, received success")),
    }
}

fn validate_visibility(
    check: &'static str,
    consistency: MemoryConsistency,
    visibility: &MemoryVisibility,
) -> Result<(), MemoryStoreContractViolation> {
    match (consistency, visibility) {
        (MemoryConsistency::Immediate, MemoryVisibility::Visible) => Ok(()),
        (MemoryConsistency::Eventual, MemoryVisibility::Pending { checkpoint })
            if !checkpoint.trim().is_empty() =>
        {
            Ok(())
        }
        (MemoryConsistency::Eventual, MemoryVisibility::Pending { .. }) => {
            violation(check, "pending visibility requires a non-empty checkpoint")
        }
        _ => violation(
            check,
            "receipt visibility does not match the declared consistency",
        ),
    }
}

fn validate_reference(
    check: &'static str,
    reference: &KnowledgeRef,
    source_id: &str,
) -> Result<(), MemoryStoreContractViolation> {
    if reference.source_id != source_id {
        return violation(check, "receipt reference belongs to another read source");
    }
    if reference.item_id.trim().is_empty() {
        return violation(check, "receipt item ID must not be empty");
    }
    if reference
        .revision
        .as_ref()
        .is_none_or(|revision| revision.trim().is_empty())
    {
        return violation(check, "receipt must contain an exact revision");
    }
    Ok(())
}

/// Verify identity, consistency, idempotency, authorization, and cancellation.
pub async fn verify_memory_store_contract(
    store: &dyn MemoryStore,
    run: &RunContext,
    case: &MemoryStoreContractCase,
) -> Result<(), MemoryStoreContractViolation> {
    let descriptor = store.descriptor();
    if descriptor.read_source_id.trim().is_empty() {
        return violation("descriptor.read_source_id", "source ID must not be empty");
    }
    if case.expected_reference.source_id != descriptor.read_source_id {
        return violation(
            "fixture.expected_reference",
            "expected reference must belong to the descriptor read source",
        );
    }
    if case.write.provenance.run_id != run.id() {
        return violation(
            "fixture.write",
            "write provenance must belong to the contract run",
        );
    }
    if case.write.idempotency_key.trim().is_empty() {
        return violation("fixture.write", "write idempotency key must not be empty");
    }

    let allowed = KnowledgeRequestContext {
        run,
        access: &case.allowed_access,
    };
    let first = store
        .upsert(allowed, case.write.clone(), CancellationToken::new())
        .await
        .map_err(|error| MemoryStoreContractViolation {
            check: "upsert.success",
            message: format!("upsert failed with {:?}", error.code()),
        })?;
    validate_reference(
        "upsert.reference",
        &first.reference,
        &descriptor.read_source_id,
    )?;
    validate_visibility(
        "upsert.visibility",
        descriptor.consistency,
        &first.visibility,
    )?;
    if first.reference != case.expected_reference {
        return violation(
            "upsert.expected_reference",
            "upsert returned an unexpected reference",
        );
    }

    let replay = store
        .upsert(allowed, case.write.clone(), CancellationToken::new())
        .await
        .map_err(|error| MemoryStoreContractViolation {
            check: "upsert.idempotency",
            message: format!("idempotent replay failed with {:?}", error.code()),
        })?;
    if replay != first {
        return violation(
            "upsert.idempotency",
            "replaying the same idempotency key changed the receipt",
        );
    }

    let denied = KnowledgeRequestContext {
        run,
        access: &case.denied_access,
    };
    expect_error_code(
        "upsert.unauthorized",
        store
            .upsert(denied, case.write.clone(), CancellationToken::new())
            .await,
        KnowledgeErrorCode::Unauthorized,
    )?;
    let upsert_abort = CancellationToken::new();
    upsert_abort.cancel();
    expect_error_code(
        "upsert.aborted",
        store
            .upsert(allowed, case.write.clone(), upsert_abort)
            .await,
        KnowledgeErrorCode::Aborted,
    )?;

    expect_error_code(
        "delete.unauthorized",
        store
            .delete(denied, first.reference.clone(), CancellationToken::new())
            .await,
        KnowledgeErrorCode::Unauthorized,
    )?;
    let delete_abort = CancellationToken::new();
    delete_abort.cancel();
    expect_error_code(
        "delete.aborted",
        store
            .delete(allowed, first.reference.clone(), delete_abort)
            .await,
        KnowledgeErrorCode::Aborted,
    )?;

    let deleted = store
        .delete(allowed, first.reference.clone(), CancellationToken::new())
        .await
        .map_err(|error| MemoryStoreContractViolation {
            check: "delete.success",
            message: format!("delete failed with {:?}", error.code()),
        })?;
    validate_reference(
        "delete.reference",
        &deleted.reference,
        &descriptor.read_source_id,
    )?;
    validate_visibility(
        "delete.visibility",
        descriptor.consistency,
        &deleted.visibility,
    )?;
    if deleted.reference.source_id != first.reference.source_id
        || deleted.reference.item_id != first.reference.item_id
    {
        return violation(
            "delete.reference",
            "delete receipt changed the memory item identity",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashMap},
        sync::{Mutex, OnceLock},
    };

    use chrono::Utc;
    use futures::future::BoxFuture;
    use llm_harness_runtime_knowledge::{KnowledgeScope, PrincipalRef};
    use llm_harness_types::{RunId, RunRequest};

    use super::*;
    use crate::{
        MemoryDeleteReceipt, MemoryProvenance, MemoryStoreDescriptor, MemoryWriteOrigin,
        MemoryWriteReceipt,
    };

    struct ContractStore {
        records: Mutex<HashMap<String, MemoryWriteReceipt>>,
    }

    impl ContractStore {
        fn check(
            ctx: KnowledgeRequestContext<'_>,
            abort: &CancellationToken,
        ) -> Result<(), KnowledgeError> {
            if abort.is_cancelled() {
                return Err(KnowledgeError::Aborted);
            }
            if ctx.access.principal.subject != "allowed" {
                return Err(KnowledgeError::Unauthorized);
            }
            Ok(())
        }
    }

    impl MemoryStore for ContractStore {
        fn descriptor(&self) -> &MemoryStoreDescriptor {
            static DESCRIPTOR: OnceLock<MemoryStoreDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| MemoryStoreDescriptor {
                read_source_id: "memory".into(),
                consistency: MemoryConsistency::Immediate,
            })
        }

        fn upsert<'a>(
            &'a self,
            ctx: KnowledgeRequestContext<'a>,
            write: MemoryWrite,
            abort: CancellationToken,
        ) -> BoxFuture<'a, Result<MemoryWriteReceipt, KnowledgeError>> {
            Box::pin(async move {
                Self::check(ctx, &abort)?;
                let mut records = self.records.lock().unwrap();
                Ok(records
                    .entry(write.idempotency_key)
                    .or_insert_with(|| MemoryWriteReceipt {
                        reference: KnowledgeRef {
                            source_id: "memory".into(),
                            item_id: "item-1".into(),
                            revision: Some("rev-1".into()),
                        },
                        visibility: MemoryVisibility::Visible,
                    })
                    .clone())
            })
        }

        fn delete<'a>(
            &'a self,
            ctx: KnowledgeRequestContext<'a>,
            reference: KnowledgeRef,
            abort: CancellationToken,
        ) -> BoxFuture<'a, Result<MemoryDeleteReceipt, KnowledgeError>> {
            Box::pin(async move {
                Self::check(ctx, &abort)?;
                Ok(MemoryDeleteReceipt {
                    reference,
                    visibility: MemoryVisibility::Visible,
                })
            })
        }
    }

    fn access(subject: &str) -> KnowledgeAccessContext {
        KnowledgeAccessContext::new(
            KnowledgeScope::new("test"),
            PrincipalRef::new(subject, "user"),
        )
    }

    fn write(run_id: RunId) -> MemoryWrite {
        MemoryWrite {
            content: "prefers concise explanations".into(),
            kind: Some("preference".into()),
            metadata: BTreeMap::new(),
            provenance: MemoryProvenance {
                run_id,
                session_id: None,
                origin: MemoryWriteOrigin::Application {
                    operation_id: "contract".into(),
                },
                recorded_at: Utc::now(),
            },
            idempotency_key: "contract-key".into(),
            expires_at: None,
        }
    }

    #[tokio::test]
    async fn fixture_store_satisfies_shared_contract() {
        let run = RunContext::new(RunRequest::from_text("remember"));
        let store = ContractStore {
            records: Mutex::new(HashMap::new()),
        };
        verify_memory_store_contract(
            &store,
            &run,
            &MemoryStoreContractCase {
                allowed_access: access("allowed"),
                denied_access: access("denied"),
                write: write(run.id()),
                expected_reference: KnowledgeRef {
                    source_id: "memory".into(),
                    item_id: "item-1".into(),
                    revision: Some("rev-1".into()),
                },
            },
        )
        .await
        .unwrap();
    }
}
