//! UI-driven settings persistence (`<data_root>/settings.json`).
//!
//! The panel is the single configuration surface: model tiers, scout
//! interval, API key, and the project list all live here. The bin starts
//! with zero config — no settings file means "show the setup wizard".
//! The API key is stored in a separate 0600 file, never in settings.json
//! and never echoed back over the API.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::roles::ModelTier;

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub models: ModelTier,
    pub scout_interval_secs: u64,
    /// OpenAI-compatible base URL; empty = the official OpenAI API.
    pub base_url: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            models: ModelTier {
                strong: String::new(),
                main: String::new(),
                cheap: String::new(),
            },
            scout_interval_secs: 3600,
            base_url: String::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid secret component: {0}")]
    InvalidComponent(String),
    #[error("parse: {0}")]
    Parse(String),
    #[error("serialize: {0}")]
    Serialize(#[from] serde_json::Error),
}

impl Settings {
    /// Load from `<data_root>/settings.json`; `Ok(None)` when absent
    /// (first run / not yet configured).
    pub fn load(data_root: &Path) -> Result<Option<Settings>, SettingsError> {
        let path = settings_path(data_root);
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)?;
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| SettingsError::Parse(e.to_string()))?;
        crate::snapshot_storage::SnapshotPolicy::from_settings(&v)?;
        let models = v.get("models").cloned().unwrap_or(serde_json::json!({}));
        Ok(Some(Settings {
            models: ModelTier {
                strong: models
                    .get("strong")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                main: models
                    .get("main")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                cheap: models
                    .get("cheap")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
            },
            scout_interval_secs: v
                .get("scout_interval_secs")
                .and_then(|x| x.as_u64())
                .unwrap_or(3600),
            base_url: v
                .get("base_url")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
        }))
    }

    /// Atomic save (tmp + rename).
    pub fn save(&self, data_root: &Path) -> Result<(), SettingsError> {
        std::fs::create_dir_all(data_root)?;
        let mut v = serde_json::json!({
            "models": {
                "strong": self.models.strong,
                "main": self.models.main,
                "cheap": self.models.cheap,
            },
            "scout_interval_secs": self.scout_interval_secs,
            "base_url": self.base_url,
        });
        let path = settings_path(data_root);
        // Model/settings updates preserve the separately operated snapshot
        // policy in this same existing configuration document.
        if path.exists() {
            let previous: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
            crate::snapshot_storage::SnapshotPolicy::from_settings(&previous)?;
            if let Some(policy) = previous.get("snapshots") {
                v["snapshots"] = policy.clone();
            }
        }
        let tmp = data_root.join("settings.json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&v)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

fn settings_path(data_root: &Path) -> PathBuf {
    data_root.join("settings.json")
}

/// API key storage: a separate 0600 file, so it never rides along in
/// settings.json or any API response.
pub fn api_key_path(data_root: &Path) -> PathBuf {
    data_root.join("apikey")
}

pub fn save_api_key(data_root: &Path, key: &str) -> Result<(), SettingsError> {
    write_private_file(&api_key_path(data_root), key.as_bytes())?;
    Ok(())
}

pub fn save_member_api_key(
    data_root: &Path,
    team_id: &str,
    agent_id: &str,
    key: &str,
) -> Result<(), SettingsError> {
    validate_secret_component(team_id)?;
    validate_secret_component(agent_id)?;
    let path = data_root
        .join("teams")
        .join(team_id)
        .join("secrets")
        .join(agent_id);
    write_private_file(&path, key.as_bytes())?;
    Ok(())
}

pub fn load_api_key(data_root: &Path) -> Option<String> {
    std::fs::read_to_string(api_key_path(data_root))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn validate_secret_component(component: &str) -> Result<(), SettingsError> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.contains('/')
        || component.contains('\\')
        || component.contains(':')
        || component.contains('\0')
    {
        return Err(SettingsError::InvalidComponent(component.to_owned()));
    }
    Ok(())
}

pub(crate) fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "secret path has no parent",
        )
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "secret path has no file name",
        )
    })?;
    std::fs::create_dir_all(parent)?;

    let mut temporary_name = file_name.to_os_string();
    temporary_name.push(".tmp-");
    temporary_name.push(uuid::Uuid::now_v7().simple().to_string());
    let temporary = parent.join(temporary_name);
    std::fs::remove_file(&temporary).or_else(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(error)
        }
    })?;

    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    #[cfg(unix)]
    let result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .and_then(|mut file| {
            file.write_all(contents)?;
            file.flush()?;
            file.sync_all()
        });
    #[cfg(not(unix))]
    let result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .and_then(|mut file| {
            file.write_all(contents)?;
            file.flush()?;
            file.sync_all()
        });

    match result {
        Ok(()) => {
            if let Err(error) = std::fs::rename(&temporary, path) {
                let _ = std::fs::remove_file(&temporary);
                return Err(error);
            }
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "ct-settings-{}-{n}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn absent_file_is_none() {
        let root = temp_root();
        assert_eq!(Settings::load(&root).unwrap(), None);
    }

    #[test]
    fn save_load_roundtrip() {
        let root = temp_root();
        let s = Settings {
            models: ModelTier {
                strong: "glm-5.2".into(),
                main: "glm-4.7".into(),
                cheap: "glm-4.5-air".into(),
            },
            scout_interval_secs: 600,
            base_url: "https://open.bigmodel.cn/api/paas/v4".into(),
        };
        s.save(&root).unwrap();
        let loaded = Settings::load(&root).unwrap().unwrap();
        assert_eq!(loaded, s);
    }

    #[test]
    fn api_key_roundtrip_and_isolation() {
        let root = temp_root();
        assert_eq!(load_api_key(&root), None);
        save_api_key(&root, "sk-test-123").unwrap();
        assert_eq!(load_api_key(&root), Some("sk-test-123".to_string()));
        // Never inside settings.json.
        Settings::default().save(&root).unwrap();
        let text = std::fs::read_to_string(root.join("settings.json")).unwrap();
        assert!(!text.contains("sk-test"));
        // 0600 permissions on unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(api_key_path(&root))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[cfg(unix)]
    #[test]
    fn api_key_save_replaces_atomically_with_private_mode() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root();
        let path = api_key_path(&root);
        save_api_key(&root, "first-key").unwrap();
        assert_eq!(load_api_key(&root).as_deref(), Some("first-key"));

        save_api_key(&root, "second-key").unwrap();
        assert_eq!(load_api_key(&root).as_deref(), Some("second-key"));
        assert!(!root.read_dir().unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_private_file_removes_temporary_on_rename_failure() {
        let root = temp_root();
        let target = root.join("api-key");
        std::fs::create_dir_all(&target).unwrap();

        assert!(write_private_file(&target, b"secret").is_err());
        assert!(!root.read_dir().unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn member_api_key_rejects_path_components() {
        let root = temp_root();
        for component in ["", ".", "..", "a/b", "a\\b", "C:", "a:b", "a\0b"] {
            assert!(matches!(
                save_member_api_key(&root, component, "planner", "key"),
                Err(SettingsError::InvalidComponent(_))
            ));
            assert!(matches!(
                save_member_api_key(&root, "project", component, "key"),
                Err(SettingsError::InvalidComponent(_))
            ));
        }
        assert!(!root.join("teams").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn member_api_key_is_stored_privately() {
        let root = temp_root();
        save_member_api_key(&root, "project", "planner", "member-key").unwrap();
        let path = root.join("teams/project/secrets/planner");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "member-key");
        assert!(
            !root
                .join("teams/project/secrets")
                .read_dir()
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".tmp-"))
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        std::fs::remove_dir_all(&root).unwrap();
    }
}
