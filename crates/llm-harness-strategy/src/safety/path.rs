//! Path traversal protection: static + canonicalize checks.

use std::path::{Path, PathBuf};

use llm_harness_types::{EnvError, ExecutionEnv};
use tokio_util::sync::CancellationToken;

// ── PathViolation ──────────────────────────────────────────────────────────

/// A path-traversal violation detected by static or canonicalize checks.
pub enum PathViolation {
    /// Path components with `..` escape the working directory.
    TraversalEscape { path: String, resolved: PathBuf },
    /// Absolute path outside the allowed root.
    AbsoluteOutsideRoot { path: String, root: PathBuf },
    /// Empty or invalid path.
    InvalidPath { path: String },
    /// Canonicalize check failed (e.g. permission denied).
    CanonicalizeFailed(String),
}

impl std::fmt::Display for PathViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathViolation::TraversalEscape { path, resolved } => {
                write!(
                    f,
                    "path escapes working directory: {path} → {}",
                    resolved.display()
                )
            }
            PathViolation::AbsoluteOutsideRoot { path, root } => {
                write!(
                    f,
                    "absolute path outside root: {path} (root: {})",
                    root.display()
                )
            }
            PathViolation::InvalidPath { path } => {
                write!(f, "invalid path: {path}")
            }
            PathViolation::CanonicalizeFailed(msg) => {
                write!(f, "canonicalize check failed: {msg}")
            }
        }
    }
}

// ── PathSafetyConfig ───────────────────────────────────────────────────────

/// Configuration for path traversal checks.
#[derive(Clone, Default)]
pub struct PathSafetyConfig {
    /// Allowed root directory. `None` = use `ExecutionEnv::working_dir()`.
    pub root: Option<PathBuf>,
    /// Whether absolute paths are allowed (default false).
    pub allow_absolute: bool,
    /// Additional denied path prefixes (e.g. `/etc`, `/var`).
    pub deny_prefixes: Vec<PathBuf>,
}

impl PathSafetyConfig {
    /// Create with defaults: root=None, allow_absolute=false, deny_prefixes=[].
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the allowed root directory.
    pub fn root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = Some(root.into());
        self
    }

    /// Allow or disallow absolute paths.
    pub fn allow_absolute(mut self, allow: bool) -> Self {
        self.allow_absolute = allow;
        self
    }

    /// Add a denied path prefix.
    pub fn deny_prefix(mut self, prefix: impl Into<PathBuf>) -> Self {
        self.deny_prefixes.push(prefix.into());
        self
    }
}

/// Static path traversal check (synchronous, pure path operations).
///
/// Checks:
/// - Empty path → InvalidPath
/// - Absolute path → if not allowed or outside root → AbsoluteOutsideRoot
/// - `..` components exceeding preceding normal components → TraversalEscape
/// - Denied prefixes → AbsoluteOutsideRoot
pub fn static_check(
    path: &str,
    working_dir: &Path,
    config: &PathSafetyConfig,
) -> Result<(), PathViolation> {
    if path.trim().is_empty() {
        return Err(PathViolation::InvalidPath {
            path: path.to_string(),
        });
    }

    let root = config.root.as_deref().unwrap_or(working_dir);
    let p = Path::new(path);

    // Check denied prefixes first.
    for prefix in &config.deny_prefixes {
        if p.starts_with(prefix) {
            return Err(PathViolation::AbsoluteOutsideRoot {
                path: path.to_string(),
                root: prefix.clone(),
            });
        }
    }

    use std::path::Component;

    if p.is_absolute() {
        if !config.allow_absolute {
            return Err(PathViolation::AbsoluteOutsideRoot {
                path: path.to_string(),
                root: root.to_path_buf(),
            });
        }
        // Absolute allowed, but must be inside root. `Path::starts_with`
        // does NOT resolve `..`, so we verify the path lexically begins
        // with root's components and then that no `..` drops below root.
        if !p.starts_with(root) {
            return Err(PathViolation::AbsoluteOutsideRoot {
                path: path.to_string(),
                root: root.to_path_buf(),
            });
        }
        // Walk path components tracking depth below the filesystem root.
        // root_depth = depth of the allowed root; a `..` that would drop
        // the path below root_depth escapes root. `Path::starts_with` does
        // NOT resolve `..`, so this depth check is required.
        let root_depth = root
            .components()
            .filter(|c| matches!(c, Component::Normal(_)))
            .count();
        let mut depth = 0i32;
        for comp in p.components() {
            match comp {
                Component::Normal(_) | Component::CurDir => depth += 1,
                Component::ParentDir => {
                    if depth <= root_depth as i32 {
                        return Err(PathViolation::TraversalEscape {
                            path: path.to_string(),
                            resolved: root.join(p),
                        });
                    }
                    depth -= 1;
                }
                Component::RootDir | Component::Prefix(_) => {}
            }
        }
        return Ok(());
    }

    // Relative path: check `..` escape by tracking component depth.
    // `Path::starts_with` does NOT resolve `..`, so we count components
    // and reject when a `..` would drop below the root.
    let mut depth = 0i32;
    for comp in p.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => depth += 1,
            Component::ParentDir => {
                if depth == 0 {
                    return Err(PathViolation::TraversalEscape {
                        path: path.to_string(),
                        resolved: root.join(p),
                    });
                }
                depth -= 1;
            }
            // Relative paths have no RootDir/Prefix; if present, ignore.
            Component::RootDir | Component::Prefix(_) => {}
        }
    }

    Ok(())
}
/// Runtime canonicalize check: resolve the path via `ExecutionEnv::file_info`
/// and verify the canonical path is inside the working directory.
///
/// If the path does not exist (file_info returns an error), this check is
/// skipped — static check has already covered `..` escape. Symbolic links
/// only matter for existing paths.
pub async fn canonicalize_check(
    path: &str,
    working_dir: &Path,
    env: &dyn ExecutionEnv,
    config: &PathSafetyConfig,
) -> Result<(), PathViolation> {
    let root = config.root.as_deref().unwrap_or(working_dir);
    let p = Path::new(path);
    let full = if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    };

    let abort = CancellationToken::new();
    match env.file_info(&full, abort).await {
        Ok(info) => {
            if !info.path.starts_with(root) {
                return Err(PathViolation::TraversalEscape {
                    path: path.to_string(),
                    resolved: info.path,
                });
            }
            Ok(())
        }
        Err(EnvError::NotFound(_)) => Ok(()),
        Err(e) => Err(PathViolation::CanonicalizeFailed(e.to_string())),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> PathSafetyConfig {
        PathSafetyConfig::new()
    }

    #[test]
    fn static_check_allows_relative_path() {
        let root = Path::new("/workspace");
        let cfg = default_config();
        assert!(static_check("src/main.rs", root, &cfg).is_ok());
    }

    #[test]
    fn static_check_denies_traversal_escape() {
        let root = Path::new("/workspace");
        let cfg = default_config();
        assert!(static_check("../../etc/passwd", root, &cfg).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn static_check_denies_absolute_outside_root() {
        let root = Path::new("/workspace");
        let cfg = default_config();
        let result = static_check("/etc/passwd", root, &cfg);
        assert!(matches!(
            result,
            Err(PathViolation::AbsoluteOutsideRoot { .. })
        ));
    }

    #[cfg(windows)]
    #[test]
    fn static_check_denies_absolute_outside_root() {
        let root = Path::new("C:\\workspace");
        let cfg = default_config();
        let result = static_check("C:\\etc\\passwd", root, &cfg);
        assert!(matches!(
            result,
            Err(PathViolation::AbsoluteOutsideRoot { .. })
        ));
    }

    #[test]
    fn static_check_denies_empty_path() {
        let root = Path::new("/workspace");
        let cfg = default_config();
        assert!(matches!(
            static_check("", root, &cfg),
            Err(PathViolation::InvalidPath { .. })
        ));
    }

    #[test]
    fn static_check_allows_absolute_when_configured() {
        let root = Path::new("/workspace");
        let cfg = PathSafetyConfig::new().allow_absolute(true);
        // /workspace/src/main.rs is inside root
        assert!(static_check("/workspace/src/main.rs", root, &cfg).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn static_check_denies_absolute_traversal_escape_when_allowed() {
        let root = Path::new("/workspace");
        let cfg = PathSafetyConfig::new().allow_absolute(true);
        // /workspace/../etc/passwd lexically starts with /workspace but
        // resolves to /etc/passwd — must be rejected.
        assert!(static_check("/workspace/../etc/passwd", root, &cfg).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn static_check_denies_absolute_traversal_escape_when_allowed() {
        let root = Path::new("C:\\workspace");
        let cfg = PathSafetyConfig::new().allow_absolute(true);
        // C:\workspace\..\etc\passwd lexically starts with C:\workspace but
        // resolves to C:\etc\passwd — must be rejected.
        assert!(static_check("C:\\workspace\\..\\etc\\passwd", root, &cfg).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn static_check_denies_absolute_outside_root_even_when_allowed() {
        let root = Path::new("/workspace");
        let cfg = PathSafetyConfig::new().allow_absolute(true);
        // /etc is outside /workspace
        assert!(static_check("/etc/passwd", root, &cfg).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn static_check_denies_absolute_outside_root_even_when_allowed() {
        let root = Path::new("C:\\workspace");
        let cfg = PathSafetyConfig::new().allow_absolute(true);
        // C:\etc is outside C:\workspace
        assert!(static_check("C:\\etc\\passwd", root, &cfg).is_err());
    }

    #[test]
    fn static_check_denies_prefix() {
        let root = Path::new("/workspace");
        let cfg = PathSafetyConfig::new().deny_prefix("/etc");
        assert!(static_check("/etc/passwd", root, &cfg).is_err());
    }

    #[test]
    fn static_check_traversal_with_normal_components() {
        let root = Path::new("/workspace");
        let cfg = default_config();
        // src/../lib is OK — still inside workspace
        assert!(static_check("src/../lib", root, &cfg).is_ok());
        // src/../../etc is NOT OK — escapes
        assert!(static_check("src/../../etc", root, &cfg).is_err());
    }
    use futures::future::BoxFuture;
    use llm_harness_types::{EnvError, FileInfo, ShellOptions, ShellOutput};
    use std::path::PathBuf;

    /// Minimal mock env that returns canonical paths for symlink simulation.
    struct MockEnv {
        working_dir: PathBuf,
        /// Map from input path to canonical FileInfo.path.
        canonical: std::collections::HashMap<PathBuf, PathBuf>,
    }

    impl ExecutionEnv for MockEnv {
        fn working_dir(&self) -> &Path {
            &self.working_dir
        }
        fn read_text_file<'a>(
            &'a self,
            _path: &'a Path,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<String, EnvError>> {
            Box::pin(async { Err(EnvError::Other("not implemented".into())) })
        }
        fn read_text_lines<'a>(
            &'a self,
            _path: &'a Path,
            _max: Option<usize>,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<Vec<String>, EnvError>> {
            Box::pin(async { Err(EnvError::Other("not implemented".into())) })
        }
        fn read_binary_file<'a>(
            &'a self,
            _path: &'a Path,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<Vec<u8>, EnvError>> {
            Box::pin(async { Err(EnvError::Other("not implemented".into())) })
        }
        fn write_file<'a>(
            &'a self,
            _path: &'a Path,
            _content: &'a [u8],
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<(), EnvError>> {
            Box::pin(async { Ok(()) })
        }
        fn append_file<'a>(
            &'a self,
            _path: &'a Path,
            _content: &'a [u8],
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<(), EnvError>> {
            Box::pin(async { Ok(()) })
        }
        fn file_info<'a>(
            &'a self,
            path: &'a Path,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<FileInfo, EnvError>> {
            let canonical = self.canonical.get(path);
            Box::pin(async move {
                match canonical {
                    Some(canon) => Ok(FileInfo {
                        path: canon.clone(),
                        is_dir: false,
                        size: 0,
                        modified: chrono::Utc::now(),
                    }),
                    None => Err(EnvError::NotFound(path.to_path_buf())),
                }
            })
        }
        fn list_dir<'a>(
            &'a self,
            _path: &'a Path,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<Vec<FileInfo>, EnvError>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn exists<'a>(
            &'a self,
            _path: &'a Path,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<bool, EnvError>> {
            Box::pin(async { Ok(false) })
        }
        fn create_dir<'a>(
            &'a self,
            _path: &'a Path,
            _recursive: bool,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<(), EnvError>> {
            Box::pin(async { Ok(()) })
        }
        fn remove<'a>(
            &'a self,
            _path: &'a Path,
            _recursive: bool,
            _force: bool,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<(), EnvError>> {
            Box::pin(async { Ok(()) })
        }
        fn create_temp_dir<'a>(
            &'a self,
            _prefix: &'a str,
        ) -> BoxFuture<'a, Result<PathBuf, EnvError>> {
            Box::pin(async { Ok(PathBuf::from("/tmp/mock")) })
        }
        fn execute_shell<'a>(
            &'a self,
            _cmd: &'a str,
            _opts: ShellOptions<'a>,
        ) -> BoxFuture<'a, Result<ShellOutput, EnvError>> {
            Box::pin(async {
                Ok(ShellOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                    stdout_truncated_bytes: 0,
                    stderr_truncated_bytes: 0,
                })
            })
        }
        fn cleanup<'a>(&'a self) -> BoxFuture<'a, Result<(), EnvError>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn canonicalize_allows_inside_root() {
        let env = MockEnv {
            working_dir: PathBuf::from("/workspace"),
            canonical: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    PathBuf::from("/workspace/src/main.rs"),
                    PathBuf::from("/workspace/src/main.rs"),
                );
                m
            },
        };
        let cfg = PathSafetyConfig::new();
        let result = canonicalize_check("src/main.rs", Path::new("/workspace"), &env, &cfg).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn canonicalize_denies_symlink_escape() {
        let env = MockEnv {
            working_dir: PathBuf::from("/workspace"),
            canonical: {
                let mut m = std::collections::HashMap::new();
                // "link" resolves to /etc/passwd (symlink escape)
                m.insert(
                    PathBuf::from("/workspace/link"),
                    PathBuf::from("/etc/passwd"),
                );
                m
            },
        };
        let cfg = PathSafetyConfig::new();
        let result = canonicalize_check("link", Path::new("/workspace"), &env, &cfg).await;
        assert!(matches!(result, Err(PathViolation::TraversalEscape { .. })));
    }

    #[tokio::test]
    async fn canonicalize_skips_nonexistent_path() {
        let env = MockEnv {
            working_dir: PathBuf::from("/workspace"),
            canonical: std::collections::HashMap::new(), // nothing exists
        };
        let cfg = PathSafetyConfig::new();
        // Path doesn't exist — canonicalize returns Ok (rely on static check)
        let result = canonicalize_check("new_file.txt", Path::new("/workspace"), &env, &cfg).await;
        assert!(result.is_ok());
    }
}
