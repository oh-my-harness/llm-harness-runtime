//! Sandbox file operations exposed to the mobile UI (upload/read/info/list).
//!
//! Guest paths may be absolute or relative to the sandbox working directory;
//! the gateway still enforces the sandbox path boundary.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::MobileError;

/// File metadata exposed to the mobile UI.
#[derive(Debug, Clone, uniffi::Record)]
pub struct MobileFileInfo {
    /// Absolute guest-side path.
    pub path: String,
    /// True when the entry is a directory.
    pub is_dir: bool,
    /// File size in bytes; directories report zero.
    pub size: u64,
    /// Last modification time in Unix milliseconds.
    pub modified_at_ms: u64,
}

pub(crate) fn resolve_sandbox_path(
    env: &Arc<dyn llm_harness_types::ExecutionEnv>,
    guest_path: &str,
) -> Result<PathBuf, MobileError> {
    let value = guest_path.trim();
    if value.is_empty() {
        return Err(MobileError::Internal {
            detail: "file path is empty".to_owned(),
        });
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(env.working_dir().join(path))
}

pub(crate) fn resolve_sandbox_directory(
    env: &Arc<dyn llm_harness_types::ExecutionEnv>,
    guest_path: &str,
) -> Result<PathBuf, MobileError> {
    if guest_path.trim().is_empty() {
        return Ok(env.working_dir().to_path_buf());
    }
    resolve_sandbox_path(env, guest_path)
}

pub(crate) fn mobile_file_info(info: llm_harness_types::FileInfo) -> MobileFileInfo {
    MobileFileInfo {
        path: info.path.to_string_lossy().into_owned(),
        is_dir: info.is_dir,
        size: info.size,
        modified_at_ms: u64::try_from(info.modified.timestamp_millis()).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::UnsupportedEnv;
    use std::sync::Arc;

    #[test]
    fn resolves_relative_paths_against_sandbox_working_dir() {
        let env: Arc<dyn llm_harness_types::ExecutionEnv> =
            Arc::new(UnsupportedEnv::with_working_dir("/workspace"));
        assert_eq!(
            resolve_sandbox_path(&env, "result.txt").unwrap(),
            PathBuf::from("/workspace/result.txt")
        );
    }

    #[test]
    fn keeps_absolute_guest_paths_unchanged() {
        let env: Arc<dyn llm_harness_types::ExecutionEnv> =
            Arc::new(UnsupportedEnv::with_working_dir("/workspace"));
        assert_eq!(
            resolve_sandbox_path(&env, "/workspace/result.txt").unwrap(),
            PathBuf::from("/workspace/result.txt")
        );
    }

    #[test]
    fn rejects_empty_file_paths() {
        let env: Arc<dyn llm_harness_types::ExecutionEnv> =
            Arc::new(UnsupportedEnv::with_working_dir("/workspace"));
        assert!(resolve_sandbox_path(&env, "   ").is_err());
    }

    #[test]
    fn resolves_empty_directory_to_working_dir() {
        let env: Arc<dyn llm_harness_types::ExecutionEnv> =
            Arc::new(UnsupportedEnv::with_working_dir("/workspace"));
        assert_eq!(
            resolve_sandbox_directory(&env, "  ").unwrap(),
            PathBuf::from("/workspace")
        );
    }

    #[test]
    fn converts_file_info_for_ffi() {
        let info = llm_harness_types::FileInfo {
            path: PathBuf::from("/workspace/result.txt"),
            is_dir: false,
            size: 12,
            modified: chrono::DateTime::from_timestamp_millis(1_700_000_000_000)
                .expect("valid timestamp"),
        };
        let mobile = mobile_file_info(info);
        assert_eq!(mobile.path, "/workspace/result.txt");
        assert!(!mobile.is_dir);
        assert_eq!(mobile.size, 12);
        assert_eq!(mobile.modified_at_ms, 1_700_000_000_000);
    }
}
