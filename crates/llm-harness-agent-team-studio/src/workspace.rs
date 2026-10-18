//! Explicit workspace authorization for host-exposed filesystem operations.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceGrant {
    pub id: String,
    pub path: PathBuf,
    pub label: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("workspace path must be an existing directory: {0}")]
    InvalidPath(PathBuf),
    #[error("workspace grant storage failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("workspace grant serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("unknown workspace grant: {0}")]
    UnknownGrant(String),
}

#[derive(Debug, Clone)]
pub struct WorkspaceGrants {
    path: PathBuf,
    grants: Vec<WorkspaceGrant>,
}

/// Strip a WSL UNC prefix (`\\wsl.localhost\<distro>\` or `\\wsl$\<distro>\`,
/// case-insensitive, either slash style) and return the remaining Linux path.
/// Returns `None` when the input is not such a UNC path; arbitrary Windows
/// drive paths (`C:\...`) are deliberately not translated and keep failing.
fn strip_wsl_unc_prefix(raw: &Path) -> Option<PathBuf> {
    let text = raw.to_string_lossy();
    if !(text.starts_with("\\\\") || text.starts_with("//")) {
        return None;
    }
    let trimmed = text.trim_start_matches(['\\', '/']);
    let mut parts = trimmed.split(['\\', '/']);
    let host = parts.next()?;
    if !host.eq_ignore_ascii_case("wsl.localhost") && !host.eq_ignore_ascii_case("wsl$") {
        return None;
    }
    let distro = parts.next()?;
    if distro.is_empty() {
        return None;
    }
    let tail: Vec<&str> = parts.filter(|segment| !segment.is_empty()).collect();
    if tail.is_empty() {
        return None;
    }
    Some(PathBuf::from("/").join(tail.join("/")))
}

/// Canonicalize a client-supplied workspace path into the single runtime
/// identity used for grants and authorization.
///
/// WSL UNC spellings (`\\wsl.localhost\Ubuntu\home\...`, `//wsl$/Debian/...`)
/// are normalized to their Linux form before canonicalizing; plain Linux
/// paths pass through unchanged. Every workspace identity check must go
/// through this helper so all client representations converge on one path.
pub fn canonicalize_client_workspace_path(
    raw: impl AsRef<Path>,
) -> Result<PathBuf, std::io::Error> {
    let raw = raw.as_ref();
    match strip_wsl_unc_prefix(raw) {
        Some(stripped) => std::fs::canonicalize(stripped),
        None => std::fs::canonicalize(raw),
    }
}

impl WorkspaceGrants {
    pub fn load(data_root: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        let data_root = data_root.as_ref();
        std::fs::create_dir_all(data_root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(data_root, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = data_root.join("workspace-grants.json");
        let grants = if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            serde_json::from_str::<Vec<WorkspaceGrant>>(&text)?
        } else {
            Vec::new()
        };
        Ok(Self { path, grants })
    }

    pub fn grants(&self) -> &[WorkspaceGrant] {
        &self.grants
    }

    pub fn add(
        &mut self,
        requested_path: impl AsRef<Path>,
        label: Option<String>,
    ) -> Result<WorkspaceGrant, WorkspaceError> {
        let requested_path = requested_path.as_ref();
        let canonical_path = canonicalize_client_workspace_path(requested_path)
            .map_err(|_| WorkspaceError::InvalidPath(requested_path.to_path_buf()))?;
        if !canonical_path.is_dir() {
            return Err(WorkspaceError::InvalidPath(canonical_path));
        }
        if self.grants.iter().any(|grant| grant.path == canonical_path) {
            return Err(WorkspaceError::InvalidPath(canonical_path));
        }
        let grant = WorkspaceGrant {
            id: uuid::Uuid::new_v4().to_string(),
            path: canonical_path,
            label: label.filter(|label| !label.trim().is_empty()),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        self.grants.push(grant.clone());
        self.save()?;
        Ok(grant)
    }

    pub fn remove(&mut self, id: &str) -> Result<WorkspaceGrant, WorkspaceError> {
        let position = self
            .grants
            .iter()
            .position(|grant| grant.id == id)
            .ok_or_else(|| WorkspaceError::UnknownGrant(id.to_string()))?;
        let grant = self.grants.remove(position);
        self.save()?;
        Ok(grant)
    }

    pub fn contains(&self, requested_path: impl AsRef<Path>) -> Result<bool, WorkspaceError> {
        let canonical_path = match canonicalize_client_workspace_path(requested_path.as_ref()) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        Ok(self
            .grants
            .iter()
            .any(|grant| canonical_path.starts_with(&grant.path)))
    }

    /// Resolve to the canonical path that will actually be used by host I/O.
    ///
    /// Returning the canonical path avoids the check/use race where a caller
    /// authorizes one spelling of a directory and then opens a symlinked
    /// spelling of that same location.
    pub fn resolve_if_authorized(
        &self,
        requested_path: impl AsRef<Path>,
    ) -> Result<Option<PathBuf>, WorkspaceError> {
        let canonical_path = match canonicalize_client_workspace_path(requested_path.as_ref()) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let authorized = self
            .grants
            .iter()
            .any(|grant| canonical_path.starts_with(&grant.path));
        Ok(authorized.then_some(canonical_path))
    }

    pub fn contains_path_with_root(
        requested_path: impl AsRef<Path>,
        root: impl AsRef<Path>,
    ) -> Result<bool, WorkspaceError> {
        let canonical_path = canonicalize_client_workspace_path(requested_path)?;
        Ok(canonical_path.starts_with(root))
    }

    fn save(&self) -> Result<(), WorkspaceError> {
        crate::settings::write_private_file(
            &self.path,
            serde_json::to_vec_pretty(&self.grants)?.as_slice(),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(test: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "agent-team-studio-workspace-{test}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn grant_is_canonicalized_persisted_and_revoked() {
        let root = temp_root("persist");
        std::fs::create_dir_all(root.join("workspace/sub")).unwrap();
        let mut grants = WorkspaceGrants::load(&root).unwrap();
        let requested = root.join("workspace");
        let grant = grants.add(&requested, Some("primary".into())).unwrap();

        assert_eq!(grant.path, std::fs::canonicalize(&requested).unwrap());
        assert_eq!(grant.label.as_deref(), Some("primary"));
        let mut reloaded = WorkspaceGrants::load(&root).unwrap();
        assert_eq!(reloaded.grants(), grants.grants());
        assert!(reloaded.contains(root.join("workspace/sub")).unwrap());
        assert!(!reloaded.contains(&root).unwrap());

        let removed = reloaded.remove(&grant.id).unwrap();
        assert_eq!(removed.id, grant.id);
        assert!(WorkspaceGrants::load(&root).unwrap().grants().is_empty());
        assert!(!root.read_dir().unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.join("workspace-grants.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn duplicate_canonical_paths_are_rejected() {
        let root = temp_root("duplicate");
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        let mut grants = WorkspaceGrants::load(&root).unwrap();
        grants.add(root.join("workspace"), None).unwrap();
        assert!(matches!(
            grants.add(root.join("workspace"), None),
            Err(WorkspaceError::InvalidPath(_))
        ));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_not_authorized() {
        let root = temp_root("symlink");
        std::fs::create_dir_all(root.join("workspace/sub")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        std::os::unix::fs::symlink(root.join("outside"), root.join("workspace/escape")).unwrap();
        let mut grants = WorkspaceGrants::load(&root).unwrap();
        grants.add(root.join("workspace"), None).unwrap();

        assert!(grants.contains(root.join("workspace/sub")).unwrap());
        assert!(!grants.contains(root.join("workspace/escape")).unwrap());
        assert!(
            grants
                .resolve_if_authorized(root.join("workspace/sub"))
                .unwrap()
                .is_some()
        );
        assert!(
            grants
                .resolve_if_authorized(root.join("workspace/escape"))
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn wsl_unc_path_converges_to_canonical_linux_identity() {
        let root = temp_root("wsl-unc");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(workspace.join("sub")).unwrap();
        let canonical = std::fs::canonicalize(&workspace).unwrap();

        // Build the UNC spelling of the real canonical path: prefix the
        // distro portion, never hard-coding the runtime's own host values.
        let tail = canonical
            .strip_prefix("/")
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let unc_host = r"\\wsl.localhost\TestDistro";
        let unc_dollars = r"\\WSL$\Other-Distro".to_string();
        let unc = format!("{unc_host}\\{}", tail.replace('/', "\\"));
        let unc_slashes = format!("{unc_dollars}/{}", tail);

        let mut grants = WorkspaceGrants::load(&root).unwrap();
        // Raw UNC -> grant succeeds, stored path is canonical Linux.
        let grant = grants.add(&unc, None).unwrap();
        assert_eq!(grant.path, canonical);
        assert!(grants.contains(&unc_slashes).unwrap());
        assert!(grants.contains(workspace.join("sub")).unwrap());
        assert!(grants.contains(&unc).unwrap());

        // Same raw UNC authorizes resolution, converging to one identity.
        assert_eq!(
            grants.resolve_if_authorized(&unc_slashes).unwrap(),
            Some(canonical.clone())
        );
        // Duplicate via a different spelling is rejected.
        assert!(matches!(
            grants.add(unc_slashes.clone(), None),
            Err(WorkspaceError::InvalidPath(_))
        ));

        // Windows drive paths are deliberately not translated.
        assert!(
            canonicalize_client_workspace_path(r"C:\Users\demo\repo").is_err()
                || cfg!(target_os = "windows")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
