//! Operator-owned host grants, independent of agent-editable team specs.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const FILE: &str = "trusted-host-grants.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostGrant {
    pub team_id: String,
    pub workspace: PathBuf,
    pub authorized_at: String,
}

fn invalid(message: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(message.to_string())
}

fn validate_id(id: &str) -> std::io::Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(invalid("invalid team id"));
    }
    Ok(())
}

/// Do not bind the policy store (or the rest of Runtime state) into any
/// agent workspace. The per-team data-dir fallback remains supported.
pub fn protect_policy_store(data_root: &Path, workspace: &Path) -> std::io::Result<()> {
    let data_root = std::fs::canonicalize(data_root)?;
    let workspace = std::fs::canonicalize(workspace)?;
    if data_root.starts_with(&workspace) {
        return Err(invalid("workspace must not contain the Runtime data root"));
    }
    Ok(())
}

pub fn grants(data_root: &Path) -> std::io::Result<Vec<HostGrant>> {
    let path = data_root.join(FILE);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(invalid("host grant store must be a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(invalid(
                "host grant store must be private and owned by the Runtime user",
            ));
        }
    }
    serde_json::from_slice(&std::fs::read(path)?).map_err(invalid)
}

pub fn is_authorized(data_root: &Path, team_id: &str, workspace: &Path) -> std::io::Result<bool> {
    let workspace = std::fs::canonicalize(workspace)?;
    Ok(grants(data_root)?
        .iter()
        .any(|g| g.team_id == team_id && g.workspace == workspace))
}

/// Called by the operator CLI, never exposed as an agent tool or HTTP API.
/// A workspace browsing grant is necessary but does not itself authorize
/// unrestricted execution. This separate grant is bound to an existing team.
pub fn authorize(data_root: &Path, team_id: &str) -> std::io::Result<HostGrant> {
    validate_id(team_id)?;
    if std::env::var_os("STUDIO_EVALUATION").is_some() {
        return Err(invalid("host authorization is disabled in evaluation mode"));
    }
    let spec: crate::team_spec::TeamSpec = serde_json::from_slice(&std::fs::read(
        data_root.join("teams").join(team_id).join("team.json"),
    )?)
    .map_err(invalid)?;
    if spec.id != team_id {
        return Err(invalid("persisted team id mismatch"));
    }
    let workspace = crate::workspace::canonicalize_client_workspace_path(
        spec.repo_path
            .ok_or_else(|| invalid("host execution requires an explicit workspace"))?,
    )?;
    protect_policy_store(data_root, &workspace)?;
    if !crate::workspace::WorkspaceGrants::load(data_root)
        .map_err(invalid)?
        .contains(&workspace)
        .map_err(invalid)?
    {
        return Err(invalid(
            "workspace must be authorized before granting host execution",
        ));
    }
    let mut stored = grants(data_root)?;
    stored.retain(|g| g.team_id != team_id);
    let grant = HostGrant {
        team_id: team_id.into(),
        workspace,
        authorized_at: chrono::Utc::now().to_rfc3339(),
    };
    stored.push(grant.clone());
    crate::settings::write_private_file(
        &data_root.join(FILE),
        &serde_json::to_vec_pretty(&stored).map_err(invalid)?,
    )?;
    Ok(grant)
}

pub fn revoke(data_root: &Path, team_id: &str) -> std::io::Result<()> {
    validate_id(team_id)?;
    let mut stored = grants(data_root)?;
    stored.retain(|g| g.team_id != team_id);
    crate::settings::write_private_file(
        &data_root.join(FILE),
        &serde_json::to_vec_pretty(&stored).map_err(invalid)?,
    )
}
