use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const AUDIT_HEAD_SUFFIX: &str = ".head";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuditEvent {
    RequestStarted,
    RequestCompleted,
    AuthenticationDenied,
    RateLimited,
    SandboxCreated,
    SandboxStarted,
    SandboxReset,
    SandboxDeleted,
    SandboxCleaned,
    ExecutionStarted,
    ExecutionKilled,
    FileRead,
    FileWritten,
    FileUploaded,
    FileRemoved,
    DirectoryCreated,
    AuditFailure,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AuditRecord {
    timestamp: DateTime<Utc>,
    event: AuditEvent,
    request_id: String,
    principal: String,
    method: String,
    route: String,
    path: String,
    status: Option<u16>,
    sandbox_id: Option<String>,
    duration_ms: Option<u64>,
    detail: serde_json::Value,
    prev_hash: Option<String>,
    hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AuditHead {
    entries: u64,
    last_hash: Option<String>,
}

pub(crate) struct GatewayAudit {
    path: PathBuf,
    head_path: PathBuf,
    state: Mutex<AuditHead>,
    degraded: Mutex<bool>,
}

impl GatewayAudit {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if !path.is_absolute() {
            anyhow::bail!("audit log path must be absolute");
        }
        let head_path = audit_head_path(&path);
        if path == head_path {
            anyhow::bail!("audit log and audit head paths must be different");
        }
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("audit log path must have a parent directory"))?;
        anyhow::ensure!(
            parent.is_dir(),
            "audit log parent directory {:?} must exist",
            parent
        );

        let chain = validate_chain(&path)?;
        if path.exists() {
            ensure_private_file(&path)?;
        } else {
            create_private_file(&path)?;
        }
        if head_path.exists() {
            ensure_private_file(&head_path)?;
        }

        if chain.entries == 0 {
            if head_path.exists() {
                let head = read_head(&head_path)?;
                if head.entries != 0 || head.last_hash.is_some() {
                    anyhow::bail!("audit log is empty but audit head is not");
                }
            } else {
                write_head(&head_path, &chain)?;
            }
        } else {
            anyhow::ensure!(
                head_path.is_file(),
                "non-empty audit log {:?} is missing its audit head",
                path
            );
            let head = read_head(&head_path)?;
            if head != chain {
                anyhow::bail!(
                    "audit head does not match audit log: head={head:?}, chain={chain:?}"
                );
            }
        }

        Ok(Self {
            path,
            head_path,
            state: Mutex::new(chain),
            degraded: Mutex::new(false),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record(
        &self,
        event: AuditEvent,
        request_id: &str,
        principal: &str,
        method: &str,
        route: &str,
        path: &str,
        status: Option<u16>,
        sandbox_id: Option<&str>,
        duration_ms: Option<u64>,
        detail: serde_json::Value,
    ) -> Result<()> {
        if *self
            .degraded
            .lock()
            .expect("gateway audit degraded lock poisoned")
        {
            anyhow::bail!("gateway audit sink is degraded");
        }

        let result = self.append_record(
            event,
            request_id,
            principal,
            method,
            route,
            path,
            status,
            sandbox_id,
            duration_ms,
            detail,
        );
        if result.is_err() {
            *self
                .degraded
                .lock()
                .expect("gateway audit degraded lock poisoned") = true;
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn append_record(
        &self,
        event: AuditEvent,
        request_id: &str,
        principal: &str,
        method: &str,
        route: &str,
        path: &str,
        status: Option<u16>,
        sandbox_id: Option<&str>,
        duration_ms: Option<u64>,
        detail: serde_json::Value,
    ) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .expect("gateway audit state lock poisoned");
        let mut record = AuditRecord {
            timestamp: Utc::now(),
            event,
            request_id: request_id.to_owned(),
            principal: principal.to_owned(),
            method: method.to_owned(),
            route: route.to_owned(),
            path: path.to_owned(),
            status,
            sandbox_id: sandbox_id.map(str::to_owned),
            duration_ms,
            detail,
            prev_hash: state.last_hash.clone(),
            hash: String::new(),
        };
        record.hash = compute_hash(&record, state.last_hash.as_deref())?;
        let line = serde_json::to_string(&record).context("serialize gateway audit record")?;

        let mut audit_file = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .with_context(|| format!("open gateway audit log {:?}", self.path))?;
        audit_file
            .write_all(line.as_bytes())
            .and_then(|()| audit_file.write_all(b"\n"))
            .and_then(|()| audit_file.sync_all())
            .with_context(|| format!("append to gateway audit log {:?}", self.path))?;

        let next_head = AuditHead {
            entries: state
                .entries
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("gateway audit entry count overflow"))?,
            last_hash: Some(record.hash),
        };
        write_head(&self.head_path, &next_head)?;
        *state = next_head;
        Ok(())
    }

    pub(crate) fn validate(path: &Path) -> Result<AuditHead> {
        let head_path = audit_head_path(path);
        let chain = validate_chain(path)?;
        let head = read_head(&head_path)?;
        if head != chain {
            anyhow::bail!("audit head does not match audit log: head={head:?}, chain={chain:?}");
        }
        Ok(chain)
    }
}

/// The result of validating an offline gateway audit log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditValidation {
    pub entries: u64,
    pub last_hash: Option<String>,
}

/// Validate an audit hash chain and its head file offline.
pub fn validate_audit(path: impl AsRef<Path>) -> anyhow::Result<AuditValidation> {
    let head = GatewayAudit::validate(path.as_ref())?;
    Ok(AuditValidation {
        entries: head.entries,
        last_hash: head.last_hash,
    })
}

fn audit_head_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    let mut file_name = file_name;
    file_name.push(AUDIT_HEAD_SUFFIX);
    path.with_file_name(file_name)
}

fn compute_hash(record: &AuditRecord, prev_hash: Option<&str>) -> Result<String> {
    let mut value = serde_json::to_value(record)?;
    value
        .as_object_mut()
        .expect("audit record is a JSON object")
        .remove("hash");
    let content = serde_json::to_string(&value)?;
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    if let Some(prev_hash) = prev_hash {
        hasher.update(b":");
        hasher.update(prev_hash.as_bytes());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn validate_chain(path: &Path) -> Result<AuditHead> {
    if !path.exists() {
        return Ok(AuditHead {
            entries: 0,
            last_hash: None,
        });
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("read gateway audit log {path:?}"))?;
    let mut entries = 0_u64;
    let mut last_hash = None;

    for (line_number, line) in content.lines().enumerate() {
        anyhow::ensure!(
            !line.trim().is_empty(),
            "gateway audit log {path:?} contains a blank line at {}",
            line_number + 1
        );
        let mut record: AuditRecord = serde_json::from_str(line).with_context(|| {
            format!(
                "parse gateway audit log {path:?} at line {}",
                line_number + 1
            )
        })?;
        let computed_hash = compute_hash(&record, last_hash.as_deref())?;
        anyhow::ensure!(
            record.prev_hash == last_hash,
            "gateway audit hash chain broken at line {}",
            line_number + 1
        );
        anyhow::ensure!(
            record.hash == computed_hash,
            "gateway audit hash mismatch at line {}",
            line_number + 1
        );
        last_hash = Some(record.hash.clone());
        record.hash.clear();
        entries = entries
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("gateway audit entry count overflow in {path:?}"))?;
    }

    Ok(AuditHead { entries, last_hash })
}

fn read_head(path: &Path) -> Result<AuditHead> {
    let content =
        fs::read_to_string(path).with_context(|| format!("read gateway audit head {path:?}"))?;
    serde_json::from_str(&content).with_context(|| format!("parse gateway audit head {path:?}"))
}

fn write_head(path: &Path, head: &AuditHead) -> Result<()> {
    let content = serde_json::to_string(head)?;
    #[cfg(unix)]
    let open_options = {
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true).mode(0o600);
        options
    };
    #[cfg(not(unix))]
    let open_options = OpenOptions::new();
    let mut file = open_options
        .open(path)
        .with_context(|| format!("open gateway audit head {path:?}"))?;
    file.write_all(content.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .with_context(|| format!("write gateway audit head {path:?}"))?;
    Ok(())
}

fn create_private_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("create private gateway audit file {path:?}"))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("create private gateway audit file {path:?}"))?;
        Ok(())
    }
}

fn ensure_private_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let symlink_metadata = fs::symlink_metadata(path)
            .with_context(|| format!("read gateway audit file link metadata {path:?}"))?;
        anyhow::ensure!(
            !symlink_metadata.is_symlink(),
            "gateway audit file {path:?} must not be a symlink"
        );
        let metadata = fs::metadata(path)
            .with_context(|| format!("read gateway audit file metadata {path:?}"))?;
        anyhow::ensure!(
            metadata.is_file(),
            "gateway audit file {path:?} must be a regular file"
        );
        anyhow::ensure!(
            metadata.permissions().mode() & 0o777 == 0o600,
            "gateway audit file {path:?} permissions must be 0600"
        );
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let metadata = fs::metadata(path)
            .with_context(|| format!("read gateway audit file metadata {path:?}"))?;
        anyhow::ensure!(
            metadata.is_file(),
            "gateway audit file {path:?} must be a regular file"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(audit: &GatewayAudit, detail: serde_json::Value) {
        audit
            .record(
                AuditEvent::RequestCompleted,
                "0197e0f2-6a08-7000-8000-000000000000",
                "tenant-a",
                "POST",
                "/v1/sandboxes",
                "/v1/sandboxes",
                Some(201),
                None,
                Some(7),
                detail,
            )
            .expect("append audit record");
    }

    #[test]
    fn audit_chain_is_validated_against_its_head() {
        let directory = tempfile::tempdir().expect("audit tempdir");
        let path = directory.path().join("audit.jsonl");
        let audit = GatewayAudit::new(&path).expect("audit sink");
        record(&audit, serde_json::json!({ "attempt": 1 }));
        record(&audit, serde_json::json!({ "attempt": 2 }));

        let head = GatewayAudit::validate(&path).expect("valid audit chain");
        assert_eq!(head.entries, 2);
        assert!(head.last_hash.is_some());
    }

    #[test]
    fn audit_validation_detects_a_modified_record() {
        let directory = tempfile::tempdir().expect("audit tempdir");
        let path = directory.path().join("audit.jsonl");
        let audit = GatewayAudit::new(&path).expect("audit sink");
        record(&audit, serde_json::Value::Null);
        drop(audit);

        let content = fs::read_to_string(&path).expect("audit content");
        fs::write(&path, content.replace("null", "\"modified\"")).expect("modify audit");
        assert!(GatewayAudit::validate(&path).is_err());
    }

    #[test]
    fn audit_validation_detects_head_mismatch() {
        let directory = tempfile::tempdir().expect("audit tempdir");
        let path = directory.path().join("audit.jsonl");
        let audit = GatewayAudit::new(&path).expect("audit sink");
        record(&audit, serde_json::Value::Null);
        drop(audit);

        fs::write(
            path.with_file_name("audit.jsonl.head"),
            r#"{"entries":0,"last_hash":null}"#,
        )
        .expect("modify audit head");
        assert!(GatewayAudit::validate(&path).is_err());
    }

    #[test]
    fn audit_sink_degrades_after_a_write_failure() {
        let directory = tempfile::tempdir().expect("audit tempdir");
        let path = directory.path().join("audit.jsonl");
        let audit = GatewayAudit::new(&path).expect("audit sink");
        fs::remove_file(&path).expect("remove audit file");

        assert!(
            audit
                .record(
                    AuditEvent::RequestCompleted,
                    "request",
                    "anonymous",
                    "POST",
                    "/",
                    "/",
                    None,
                    None,
                    None,
                    serde_json::Value::Null
                )
                .is_err()
        );
        assert!(
            audit
                .record(
                    AuditEvent::RequestCompleted,
                    "request",
                    "anonymous",
                    "POST",
                    "/",
                    "/",
                    None,
                    None,
                    None,
                    serde_json::Value::Null
                )
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn audit_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("audit tempdir");
        let path = directory.path().join("audit.jsonl");
        let audit = GatewayAudit::new(&path).expect("audit sink");
        record(&audit, serde_json::Value::Null);
        drop(audit);

        let mode = |file: &Path| {
            fs::metadata(file)
                .expect("audit file metadata")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&path.with_file_name("audit.jsonl.head")), 0o600);
    }
}
