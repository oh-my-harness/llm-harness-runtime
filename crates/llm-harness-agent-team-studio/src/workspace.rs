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
        let canonical_path = std::fs::canonicalize(requested_path)
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
        let canonical_path = match std::fs::canonicalize(requested_path.as_ref()) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        Ok(self
            .grants
            .iter()
            .any(|grant| canonical_path.starts_with(&grant.path)))
    }

    pub fn contains_path_with_root(
        requested_path: impl AsRef<Path>,
        root: impl AsRef<Path>,
    ) -> Result<bool, WorkspaceError> {
        let canonical_path = std::fs::canonicalize(requested_path)?;
        Ok(canonical_path.starts_with(root))
    }

    fn save(&self) -> Result<(), WorkspaceError> {
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(&self.grants)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&temporary, &self.path)?;
        #[cfg(unix)]
        restrict_file(&self.path)?;
        Ok(())
    }
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
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
        std::fs::remove_dir_all(&root).unwrap();
    }
}
