//! Integration tests for BwrapSandbox isolation.
//!
//! Tests are split into two categories:
//!
//! - **File API tests** (cap-std based, no bwrap required): always run.
//!   These verify path rejection, symlink escape prevention, and
//!   workspace-scoped file operations.
//!
//! - **Shell tests** (bwrap based): marked `#[ignore]` so a default
//!   `cargo test` reports them as ignored. Run with `--include-ignored`
//!   on a host with bwrap installed.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use llm_harness_platform::sandbox::{NetRule, ResourceLimits, SandboxConfig};
use llm_harness_types::{EnvError, ShellOptions};
use tokio_util::sync::CancellationToken;

use super::*;

fn default_config(work_dir: PathBuf) -> SandboxConfig {
    SandboxConfig {
        fs_allowlist: vec![],
        fs_denylist: vec![],
        net_allowlist: vec![],
        resource_limits: ResourceLimits {
            max_cpus: None,
            max_processes: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: Some(work_dir),
        trust_profile: Default::default(),
    }
}

/// Config with work_dir=None (sandbox creates and owns the directory).
fn owned_config() -> SandboxConfig {
    SandboxConfig {
        fs_allowlist: vec![],
        fs_denylist: vec![],
        net_allowlist: vec![],
        resource_limits: ResourceLimits {
            max_cpus: None,
            max_processes: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: None,
        trust_profile: Default::default(),
    }
}

fn shell_opts() -> ShellOptions<'static> {
    ShellOptions {
        cwd: None,
        env: vec![],
        timeout: None,
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
        max_output_bytes: None,
    }
}

/// Returns true if bwrap is installed AND can actually create a sandbox.
///
/// CI runners often have the `bwrap` binary but run inside containers that
/// forbid the namespace operations bwrap needs (e.g. `RTM_NEWADDR:
/// Operation not permitted`). A bare `--version` check passes in that case,
/// so we probe with a minimal `--unshare-all` invocation instead.
fn bwrap_available() -> bool {
    // bwrap --version 验证二进制存在
    if !std::process::Command::new("bwrap")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return false;
    }
    let mut command = std::process::Command::new("bwrap");
    command.args(["--unshare-all", "--die-with-parent", "--new-session"]);
    for (source, destination) in BwrapEnv::system_ro_binds() {
        command.arg("--ro-bind-try").arg(source).arg(destination);
    }
    command
        .arg("--proc")
        .arg("/proc")
        .arg("--dev")
        .arg("/dev")
        .arg("--tmpfs")
        .arg("/tmp")
        .arg("--")
        .arg("/usr/bin/env")
        .arg("-i")
        .arg("PATH=/usr/bin:/bin")
        .arg("true")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Checks bwrap availability; returns `false` to skip the test if bwrap
/// is absent or the kernel denies namespace creation.
///
/// Tests that need bwrap are marked `#[ignore]` so a default `cargo test`
/// reports them as ignored. When run explicitly with `--include-ignored`,
/// this guard skips gracefully if bwrap is non-functional (e.g. inside a
/// CI container that forbids namespace operations).
fn require_bwrap() -> bool {
    if !bwrap_available() {
        eprintln!("skipped: bwrap not available or namespace creation denied");
        return false;
    }
    true
}

fn cgroup_v2_pids_available() -> bool {
    let controllers =
        std::fs::read_to_string("/sys/fs/cgroup/cgroup.controllers").unwrap_or_default();
    let subtree =
        std::fs::read_to_string("/sys/fs/cgroup/cgroup.subtree_control").unwrap_or_default();
    controllers.split_whitespace().any(|c| c == "pids")
        && subtree.split_whitespace().any(|c| c == "pids")
}

// ═══════════════════════════════════════════════════════════════════════════
// Config validation (fail-closed) — no bwrap required
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn file_api_enforces_fs_denylist() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();
    let denied = work_dir.join("denied");
    let config = SandboxConfig {
        fs_allowlist: vec![],
        fs_denylist: vec![denied.clone()],
        net_allowlist: vec![],
        resource_limits: ResourceLimits {
            max_cpus: None,
            max_processes: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: Some(dir.path().to_path_buf()),
    };
    let sandbox = BwrapSandbox::new(config).unwrap();
    let error = sandbox
        .env
        .write_file(&denied, b"forbidden\n", CancellationToken::new())
        .await
        .expect_err("fs_denylist must be enforced");
    assert!(matches!(error, EnvError::PermissionDenied(_)));
}

#[test]
fn config_with_net_allowlist_rules_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let config = SandboxConfig {
        fs_allowlist: vec![],
        fs_denylist: vec![],
        net_allowlist: vec![NetRule {
            host: "example.com".into(),
            port_range: Some((443, 443)),
        }],
        resource_limits: ResourceLimits {
            max_cpus: None,
            max_processes: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: Some(dir.path().to_path_buf()),
    };
    let result = BwrapSandbox::new(config);
    assert!(result.is_err(), "net_allowlist rules must be rejected");
    assert!(result.unwrap_err().to_string().contains("net_allowlist"));
}

#[test]
fn config_with_disk_limit_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let config = SandboxConfig {
        fs_allowlist: vec![],
        fs_denylist: vec![],
        net_allowlist: vec![],
        resource_limits: ResourceLimits {
            max_cpus: None,
            max_processes: None,
            max_memory_mb: None,
            max_disk_mb: Some(1),
            timeout: None,
        },
        work_dir: Some(dir.path().to_path_buf()),
    };
    let result = BwrapSandbox::new(config);
    assert!(result.is_err(), "max_disk_mb must be rejected");
}

#[test]
fn new_owned_rejects_symlink_work_dir() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let link = directory.path().join("work");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let result = BwrapSandbox::new_owned(owned_config(), link);
    let error = result.expect_err("owned work_dir must not be a symlink");
    assert!(error.to_string().contains("must not be a symlink"));
}

#[test]
fn new_owned_rejects_work_dir_with_wrong_permissions() {
    let directory = tempfile::tempdir().unwrap();
    let work_dir = directory.path().join("work");
    std::fs::create_dir(&work_dir).unwrap();
    std::fs::set_permissions(&work_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    let result = BwrapSandbox::new_owned(owned_config(), work_dir);
    let error = result.expect_err("owned work_dir must have 0700 permissions");
    assert!(error.to_string().contains("must have 0700 permissions"));
}

// ═══════════════════════════════════════════════════════════════════════════
// File API path restrictions (cap-std) — no bwrap required
// ═══════════════════════════════════════════════════════════════════════════

/// File API rejects absolute paths outside the workspace.
#[tokio::test]
async fn file_api_rejects_absolute_path_outside_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir)).unwrap();
    let env = sandbox.env.clone();

    let result = env
        .read_text_file(Path::new("/etc/passwd"), CancellationToken::new())
        .await;
    assert!(
        result.is_err(),
        "absolute path outside workspace must be rejected"
    );
}

/// File API rejects `..` traversal.
#[tokio::test]
async fn file_api_rejects_dotdot_traversal() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    tokio::fs::write(dir.path().join("outside.txt"), "outside")
        .await
        .unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir)).unwrap();
    let env = sandbox.env.clone();

    let result = env
        .read_text_file(Path::new("../outside.txt"), CancellationToken::new())
        .await;
    assert!(result.is_err(), "`..` traversal must be rejected");
}

/// File API allows paths inside the workspace.
#[tokio::test]
async fn file_api_allows_paths_inside_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir)).unwrap();
    let env = sandbox.env.clone();

    env.write_file(
        Path::new("inside.txt"),
        b"content",
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let text = env
        .read_text_file(Path::new("inside.txt"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(text, "content");
}

/// File API allows paths inside fs_allowlist entries.
#[tokio::test]
async fn file_api_allows_fs_allowlist_paths() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    let shared_dir = dir.path().join("shared");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();
    tokio::fs::create_dir_all(&shared_dir).await.unwrap();
    tokio::fs::write(shared_dir.join("shared.txt"), "shared_content")
        .await
        .unwrap();

    let config = SandboxConfig {
        fs_allowlist: vec![shared_dir.clone()],
        fs_denylist: vec![],
        net_allowlist: vec![],
        resource_limits: ResourceLimits {
            max_cpus: None,
            max_processes: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: Some(work_dir),
    };
    let sandbox = BwrapSandbox::new(config).unwrap();
    let env = sandbox.env.clone();

    let text = env
        .read_text_file(&shared_dir.join("shared.txt"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(text, "shared_content");
}

/// P0: File API rejects symlinks pointing outside the workspace.
///
/// cap-std prevents symlink escape at the syscall level: when a symlink
/// inside the workspace points to an absolute or `..` path outside, the
/// cap-std `Dir::open` rejects it.
#[tokio::test]
async fn file_api_rejects_symlink_escape() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    // Create a secret file outside the workspace.
    let secret = dir.path().join("host-secret.txt");
    tokio::fs::write(&secret, "host_secret_value")
        .await
        .unwrap();

    // Create a symlink inside the workspace pointing to the outside file.
    let link = work_dir.join("escape");
    std::os::unix::fs::symlink(&secret, &link).unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir.clone())).unwrap();
    let env = sandbox.env.clone();

    // Reading via the symlink must fail — cap-std blocks the escape.
    let result = env
        .read_text_file(Path::new("escape"), CancellationToken::new())
        .await;
    assert!(
        result.is_err(),
        "symlink pointing outside workspace must be blocked; got: {:?}",
        result
    );

    // Also test a relative `..` symlink.
    let link2 = work_dir.join("escape_rel");
    std::os::unix::fs::symlink("../host-secret.txt", &link2).unwrap();

    let result2 = env
        .read_text_file(Path::new("escape_rel"), CancellationToken::new())
        .await;
    assert!(
        result2.is_err(),
        "relative `..` symlink escape must be blocked; got: {:?}",
        result2
    );
}

/// P0: File API allows symlinks pointing to files *inside* the workspace.
#[tokio::test]
async fn file_api_allows_internal_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    tokio::fs::write(work_dir.join("target.txt"), "internal")
        .await
        .unwrap();

    // Create a symlink pointing to a file inside the workspace.
    std::os::unix::fs::symlink("target.txt", work_dir.join("good_link")).unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir)).unwrap();
    let env = sandbox.env.clone();

    let text = env
        .read_text_file(Path::new("good_link"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(text, "internal");
}

// ═══════════════════════════════════════════════════════════════════════════
// Temp dir isolation — no bwrap required
// ═══════════════════════════════════════════════════════════════════════════

/// P1: create_temp_dir creates inside the workspace, not host /tmp.
#[tokio::test]
async fn create_temp_dir_is_inside_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir)).unwrap();
    let env = sandbox.env.clone();

    let temp = env.create_temp_dir("probe").await.unwrap();

    // The temp dir must be inside the workspace.
    assert!(
        temp.starts_with(env.working_dir()),
        "temp dir {:?} must be inside workspace {:?}",
        temp,
        env.working_dir()
    );

    // The temp dir must exist.
    assert!(temp.exists(), "temp dir must exist");

    // The temp dir must be usable (write + read a file).
    let test_file = temp.join("test.txt");
    env.write_file(&test_file, b"test", CancellationToken::new())
        .await
        .unwrap();
    let content = env
        .read_text_file(&test_file, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(content, "test");
}

// ═══════════════════════════════════════════════════════════════════════════
// ═══════════════════════════════════════════════════════════════════════════
// Reset & ownership — no bwrap required
// ═══════════════════════════════════════════════════════════════════════════

/// P0: reset() clears an owned (auto-generated) work_dir.
///
/// Only directories created by the sandbox (work_dir=None) can be reset.
/// After reset, the directory must be empty (no sentinel, no leftover files).
#[tokio::test]
async fn reset_clears_owned_work_dir() {
    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("outside.txt");
    tokio::fs::write(&outside_file, "should_survive")
        .await
        .unwrap();

    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    let env = sandbox.env.clone();
    let work_dir = env.working_dir().to_path_buf();

    env.write_file(
        Path::new("inside.txt"),
        b"should_be_deleted",
        CancellationToken::new(),
    )
    .await
    .unwrap();

    sandbox.reset().await.unwrap();

    // After reset, the work_dir must be completely empty.
    let entries: Vec<String> = std::fs::read_dir(&work_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        entries.is_empty(),
        "work dir must be empty after reset; got: {:?}",
        entries
    );

    // The outside file must survive.
    let content = tokio::fs::read_to_string(&outside_file).await.unwrap();
    assert_eq!(content, "should_survive");

    // Cleanup.
    let _ = std::fs::remove_dir_all(&work_dir);
}

/// P0: reset() refuses caller-provided directories.
///
/// The sandbox must not claim ownership of directories it did not create.
/// A caller-provided work_dir (work_dir=Some(path)) can never be reset.
#[tokio::test]
async fn reset_refuses_caller_provided_directory() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir.clone())).unwrap();
    let result = sandbox.reset().await;
    assert!(
        result.is_err(),
        "reset must refuse caller-provided directory"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("does not own"),
        "error must mention ownership; got: {}",
        err
    );

    // The directory and its contents must be untouched.
    assert!(work_dir.exists(), "caller directory must still exist");
}

/// P0: reset() refuses root path (caller-provided, not owned).
#[tokio::test]
async fn reset_refuses_root_path() {
    let config = SandboxConfig {
        fs_allowlist: vec![],
        fs_denylist: vec![],
        net_allowlist: vec![],
        resource_limits: ResourceLimits {
            max_cpus: None,
            max_processes: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: Some(PathBuf::from("/")),
    };
    let sandbox = BwrapSandbox::new(config).unwrap();
    let result = sandbox.reset().await;
    assert!(result.is_err(), "reset must refuse root path");
}

/// P0: reset() refuses /var/lib (caller-provided, not owned).
///
/// The old sentinel model would claim /var/lib by writing a sentinel.
/// The ownership model never claims caller-provided directories.
#[tokio::test]
async fn reset_refuses_var_lib() {
    let config = SandboxConfig {
        fs_allowlist: vec![],
        fs_denylist: vec![],
        net_allowlist: vec![],
        resource_limits: ResourceLimits {
            max_cpus: None,
            max_processes: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: Some(PathBuf::from("/var/lib")),
    };
    let sandbox = BwrapSandbox::new(config).unwrap();
    let result = sandbox.reset().await;
    assert!(result.is_err(), "reset must refuse /var/lib");
    assert!(
        result.unwrap_err().to_string().contains("does not own"),
        "error must mention ownership"
    );
}

/// P0: reset() refuses /tmp (caller-provided, not owned).
#[tokio::test]
async fn reset_refuses_tmp_directory() {
    let config = SandboxConfig {
        fs_allowlist: vec![],
        fs_denylist: vec![],
        net_allowlist: vec![],
        resource_limits: ResourceLimits {
            max_cpus: None,
            max_processes: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: Some(PathBuf::from("/tmp")),
    };
    let sandbox = BwrapSandbox::new(config).unwrap();
    let result = sandbox.reset().await;
    assert!(result.is_err(), "reset must refuse /tmp");
}

/// P0: reset() refuses "." (caller-provided, not owned).
#[tokio::test]
async fn reset_refuses_dot_path() {
    let config = SandboxConfig {
        fs_allowlist: vec![],
        fs_denylist: vec![],
        net_allowlist: vec![],
        resource_limits: ResourceLimits {
            max_cpus: None,
            max_processes: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: Some(PathBuf::from(".")),
    };
    let sandbox = BwrapSandbox::new(config).unwrap();
    let result = sandbox.reset().await;
    assert!(result.is_err(), "reset must refuse '.' (not owned)");
}

/// P0: After reset(), file operations still work (Dir handle not invalidated).
#[tokio::test]
async fn reset_preserves_file_api_usability() {
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    let env = sandbox.env.clone();
    let work_dir = env.working_dir().to_path_buf();

    env.write_file(Path::new("before.txt"), b"before", CancellationToken::new())
        .await
        .unwrap();

    sandbox.reset().await.unwrap();

    let result = env
        .write_file(Path::new("after.txt"), b"after", CancellationToken::new())
        .await;
    assert!(
        result.is_ok(),
        "write_file after reset must succeed; got: {:?}",
        result
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// P0: Auto-generated workspace has 0700 permissions.
///
/// create_dir_all() is subject to umask and typically yields 0755, allowing
/// other Unix users to read agent files. The sandbox must use 0700.
#[tokio::test]
async fn auto_generated_dir_has_0700_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    let work_dir = sandbox.env().working_dir().to_path_buf();

    let meta = std::fs::metadata(&work_dir).unwrap();
    let mode = meta.permissions().mode();
    assert_eq!(
        mode & 0o777,
        0o700,
        "auto-generated work_dir must have 0700 permissions; got {:04o}",
        mode & 0o777
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// P0: Agent cannot break reset by modifying the workspace.
///
/// The old sentinel model placed a sentinel file inside the agent-writable
/// workspace; the agent could delete it to permanently disable reset().
/// The ownership model tracks ownership in memory, so the agent cannot
/// tamper with it.
#[tokio::test]
async fn agent_cannot_break_reset() {
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    let env = sandbox.env.clone();
    let work_dir = env.working_dir().to_path_buf();

    // Agent writes and deletes files in the workspace.
    env.write_file(Path::new("file1.txt"), b"data", CancellationToken::new())
        .await
        .unwrap();
    env.remove(
        Path::new("file1.txt"),
        false,
        true,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    // Reset must still work — ownership is in memory, not the filesystem.
    sandbox.reset().await.unwrap();

    let entries: Vec<String> = std::fs::read_dir(&work_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert!(entries.is_empty(), "work dir must be empty after reset");

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// P1: shutdown() deletes owned (auto-generated) directories.
#[tokio::test]
async fn shutdown_deletes_owned_directory() {
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    let work_dir = sandbox.env().working_dir().to_path_buf();
    assert!(work_dir.exists(), "work_dir must exist before shutdown");

    Box::new(sandbox).shutdown().await.unwrap();

    assert!(
        !work_dir.exists(),
        "owned work_dir must be deleted by shutdown"
    );
}

/// P1: shutdown() does NOT delete caller-provided directories.
#[tokio::test]
async fn shutdown_preserves_caller_directory() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir.clone())).unwrap();
    Box::new(sandbox).shutdown().await.unwrap();

    assert!(
        work_dir.exists(),
        "caller-provided work_dir must NOT be deleted by shutdown"
    );
}

// Lifecycle — bwrap required
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn bwrap_sandbox_starts_when_available() {
    if !require_bwrap() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sandbox = BwrapSandbox::new(default_config(dir.path().to_path_buf())).unwrap();
    sandbox.start().await.unwrap();
    assert!(sandbox.is_running());
    Box::new(sandbox).shutdown().await.unwrap();
}

/// Verifies the bwrap probe fails when the binary is missing.
///
/// This works on any machine (no bwrap required) and exercises the
/// "not found" error path that `start()` relies on. The previous
/// `start_fails_when_unavailable` test required bwrap to be absent and
/// panicked on bwrap-present hosts, which broke `--include-ignored` in CI.
#[tokio::test]
async fn bwrap_probe_fails_when_binary_missing() {
    let result = BwrapSandbox::run_bwrap_probe("/nonexistent/bwrap-binary").await;
    assert!(
        result.is_err(),
        "probe must fail when bwrap binary is missing"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("not available"),
        "error must mention bwrap not available; got: {}",
        err_msg
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Shell isolation — bwrap required
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn shell_cannot_read_host_file_outside_allowlist() {
    if !require_bwrap() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let secret_path = dir.path().join("host-secret.txt");
    tokio::fs::write(&secret_path, "host_secret_value")
        .await
        .unwrap();

    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir.clone())).unwrap();
    sandbox.start().await.unwrap();

    let env = sandbox.env.clone();
    let cmd = format!(
        "cat {} 2>/dev/null && echo LEAKED || echo BLOCKED",
        secret_path.to_string_lossy()
    );
    let opts = ShellOptions {
        cwd: None,
        env: vec![],
        timeout: Some(Duration::from_secs(10)),
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
        max_output_bytes: None,
    };
    let result = env.execute_shell(&cmd, opts).await.unwrap();
    assert!(
        result.stdout.contains("BLOCKED"),
        "sandbox must prevent reading host files; got: {}",
        result.stdout
    );
}

#[test]
fn mapped_policy_paths_reject_symlink_escape() {
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "host_secret_value").unwrap();
    let work_dir = dir.path().join("workspace");
    std::fs::create_dir_all(&work_dir).unwrap();
    std::fs::set_permissions(&work_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink(&outside, work_dir.join("allowed")).unwrap();

    let config = SandboxConfig {
        fs_allowlist: vec![PathBuf::from("/workspace/allowed")],
        fs_denylist: vec![],
        net_allowlist: vec![],
        resource_limits: ResourceLimits {
            max_cpus: None,
            max_processes: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: None,
    };
    let error = BwrapSandbox::new_mapped(config, work_dir, PathBuf::from("/workspace"))
        .expect_err("mapped policy paths must reject symlinks");
    assert!(error.to_string().contains("contains a symlink"));
}

#[tokio::test]
#[ignore = "requires bwrap and delegated cgroup v2 pids controller"]
async fn shell_enforces_cgroup_process_limit() {
    if !require_bwrap() || !cgroup_v2_pids_available() {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let mut config = default_config(dir.path().to_path_buf());
    config.resource_limits.max_processes = Some(1);
    let sandbox = BwrapSandbox::new(config).unwrap();
    let env = sandbox.env.clone();

    let result = env.execute_shell("sleep 0.2 & wait", shell_opts()).await;
    assert!(result.is_err(), "pids.max=1 must reject descendant fork");
    Box::new(sandbox).shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn shell_cannot_read_etc_passwd() {
    if !require_bwrap() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir)).unwrap();
    sandbox.start().await.unwrap();

    let env = sandbox.env.clone();
    let cmd = "cat /etc/passwd 2>/dev/null && echo LEAKED || echo BLOCKED";
    let opts = ShellOptions {
        cwd: None,
        env: vec![],
        timeout: Some(Duration::from_secs(10)),
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
        max_output_bytes: None,
    };
    let result = env.execute_shell(cmd, opts).await.unwrap();
    assert!(
        result.stdout.contains("BLOCKED"),
        "/etc/passwd must not be readable; got: {}",
        result.stdout
    );
}

#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn workspace_files_are_accessible_from_sandbox() {
    if !require_bwrap() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();
    tokio::fs::write(work_dir.join("readable.txt"), "workspace_content")
        .await
        .unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir.clone())).unwrap();
    sandbox.start().await.unwrap();

    let env = sandbox.env.clone();
    let result = env
        .execute_shell("cat readable.txt", shell_opts())
        .await
        .unwrap();
    assert_eq!(result.stdout.trim(), "workspace_content");
}

#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn empty_net_allowlist_blocks_network() {
    if !require_bwrap() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir)).unwrap();
    sandbox.start().await.unwrap();

    let env = sandbox.env.clone();
    let cmd = "timeout 3 bash -c 'echo test > /dev/tcp/8.8.8.8/53' 2>&1 && echo NETWORK_ALLOWED || echo NETWORK_BLOCKED";
    let opts = ShellOptions {
        cwd: None,
        env: vec![],
        timeout: Some(Duration::from_secs(15)),
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
        max_output_bytes: None,
    };
    let result = env.execute_shell(cmd, opts).await.unwrap();
    assert!(
        result.stdout.contains("NETWORK_BLOCKED"),
        "empty net_allowlist must block network; got: {}",
        result.stdout
    );
}

#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn host_credentials_not_inherited() {
    if !require_bwrap() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir)).unwrap();
    sandbox.start().await.unwrap();

    let env = sandbox.env.clone();
    let opts = ShellOptions {
        cwd: None,
        env: vec![("SANDBOX_INJECTED", "injected_value")],
        timeout: Some(Duration::from_secs(10)),
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
        max_output_bytes: None,
    };
    let result = env.execute_shell("env | sort", opts).await.unwrap();

    assert!(
        result.stdout.contains("SANDBOX_INJECTED=injected_value"),
        "injected var must be present; got:\n{}",
        result.stdout
    );

    for host_var in &["USER", "LANG", "HOSTNAME", "LOGNAME", "TERM"] {
        if std::env::var(host_var).is_ok() {
            assert!(
                !result.stdout.contains(&format!("{host_var}=")),
                "host var {host_var} must not leak; got:\n{}",
                result.stdout
            );
        }
    }
}

#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn injected_env_vars_are_visible_in_sandbox() {
    if !require_bwrap() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir)).unwrap();
    sandbox.start().await.unwrap();

    let env = sandbox.env.clone();
    let opts = ShellOptions {
        cwd: None,
        env: vec![("MY_CUSTOM_VAR", "custom_value_42")],
        timeout: Some(Duration::from_secs(10)),
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
        max_output_bytes: None,
    };
    let result = env
        .execute_shell("echo $MY_CUSTOM_VAR", opts)
        .await
        .unwrap();
    assert_eq!(result.stdout.trim(), "custom_value_42");
}

// ═══════════════════════════════════════════════════════════════════════════
// Process tree & timeout — bwrap required
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn timeout_terminates_entire_process_tree() {
    if !require_bwrap() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir.clone())).unwrap();
    sandbox.start().await.unwrap();

    let env = sandbox.env.clone();
    let cmd = "(sleep 3 && echo SURVIVED > sentinel.txt) & sleep 30";
    let opts = ShellOptions {
        cwd: None,
        env: vec![],
        timeout: Some(Duration::from_secs(2)),
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
        max_output_bytes: None,
    };
    let start = std::time::Instant::now();
    let result = env.execute_shell(cmd, opts).await;
    let elapsed = start.elapsed();
    assert!(result.is_err(), "timeout must produce an error");
    assert!(
        elapsed < Duration::from_secs(5),
        "timeout must kill quickly; took {elapsed:?}"
    );

    tokio::time::sleep(Duration::from_secs(3)).await;
    let sentinel = work_dir.join("sentinel.txt");
    assert!(
        !sentinel.exists(),
        "background child must have been killed — sentinel file must not exist"
    );
}

#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn abort_cancels_running_command() {
    if !require_bwrap() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir)).unwrap();
    sandbox.start().await.unwrap();

    let env = sandbox.env.clone();
    let abort = CancellationToken::new();
    let abort_clone = abort.clone();

    let handle = tokio::spawn(async move {
        let opts = ShellOptions {
            cwd: None,
            env: vec![],
            timeout: None,
            abort: abort_clone,
            on_stdout: None,
            on_stderr: None,
            max_output_bytes: None,
        };
        env.execute_shell("sleep 30", opts).await
    });

    tokio::time::sleep(Duration::from_millis(500)).await;
    abort.cancel();

    let result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("abort must cause quick return")
        .expect("task must not panic");
    assert!(result.is_err(), "abort must produce an error");
}

/// P1: Config-level timeout is enforced as an upper bound on shell commands.
///
/// When `resource_limits.timeout` is set, `execute_shell` caps the effective
/// timeout to the minimum of the caller-specified timeout and the config
/// timeout. If the caller specifies no timeout, the config timeout applies.
#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn config_timeout_enforced_as_upper_bound() {
    if !require_bwrap() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let config = SandboxConfig {
        fs_allowlist: vec![],
        fs_denylist: vec![],
        net_allowlist: vec![],
        resource_limits: ResourceLimits {
            max_cpus: None,
            max_processes: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: Some(Duration::from_secs(1)),
        },
        work_dir: Some(work_dir),
    };
    let sandbox = BwrapSandbox::new(config).unwrap();
    sandbox.start().await.unwrap();
    let env = sandbox.env.clone();

    // No per-call timeout; config timeout (1s) should apply.
    let opts = ShellOptions {
        cwd: None,
        env: vec![],
        timeout: None,
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
        max_output_bytes: None,
    };
    let start = std::time::Instant::now();
    let result = env.execute_shell("sleep 10", opts).await;
    let elapsed = start.elapsed();
    assert!(result.is_err(), "config timeout must produce an error");
    assert!(
        elapsed < Duration::from_secs(3),
        "config timeout must fire quickly; took {elapsed:?}"
    );

    // Per-call timeout (30s) must be capped to config timeout (1s).
    let opts2 = ShellOptions {
        cwd: None,
        env: vec![],
        timeout: Some(Duration::from_secs(30)),
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
        max_output_bytes: None,
    };
    let start2 = std::time::Instant::now();
    let result2 = env.execute_shell("sleep 10", opts2).await;
    let elapsed2 = start2.elapsed();
    assert!(result2.is_err(), "config timeout must cap per-call timeout");
    assert!(
        elapsed2 < Duration::from_secs(3),
        "capped timeout must fire quickly; took {elapsed2:?}"
    );
}

/// P0: work_dir=None generates a unique directory per sandbox with 0700
/// permissions to prevent cross-agent file leakage and mutual reset.
#[tokio::test]
async fn work_dir_none_generates_unique_directory() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox1 = BwrapSandbox::new(owned_config()).unwrap();
    let sandbox2 = BwrapSandbox::new(owned_config()).unwrap();

    let dir1 = sandbox1.env().working_dir().to_path_buf();
    let dir2 = sandbox2.env().working_dir().to_path_buf();

    assert_ne!(dir1, dir2, "work_dir=None must generate unique directories");

    // Both must have 0700 permissions.
    for d in [&dir1, &dir2] {
        let mode = std::fs::metadata(d).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "auto-generated dir must have 0700; got {:04o}",
            mode & 0o777
        );
    }

    // Clean up.
    let _ = std::fs::remove_dir_all(&dir1);
    let _ = std::fs::remove_dir_all(&dir2);
}

/// P0: working_dir() returns the configured path, not the canonicalized
/// path. A symlink work_dir must be preserved so callers can compare the
/// returned path against what they configured.
#[tokio::test]
async fn working_dir_returns_configured_path_not_canonical() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("real_workspace");
    tokio::fs::create_dir_all(&target).await.unwrap();
    let link = dir.path().join("symlink_workspace");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let sandbox = BwrapSandbox::new(default_config(link.clone())).unwrap();
    let env = sandbox.env.clone();

    assert_eq!(
        env.working_dir(),
        &link,
        "working_dir() must return configured path, not canonicalized"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Lifecycle regression tests — bwrap required
// ═══════════════════════════════════════════════════════════════════════════

/// P0: An unbounded shell command (no timeout) must not block shutdown().
///
/// `execute_shell()` acquires `reset_lock` and holds it through
/// `drain_child_output()`. `shutdown()` must cancel/kill the active
/// command and complete in bounded time, not wait for the lock forever.
#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn shutdown_completes_with_unbounded_command() {
    if !require_bwrap() {
        return;
    }
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    sandbox.start().await.unwrap();
    let env = sandbox.env.clone();
    let work_dir = env.working_dir().to_path_buf();

    // Start a long-running command with NO timeout and NO abort.
    let env_clone = env.clone();
    let cmd_handle = tokio::spawn(async move {
        let opts = ShellOptions {
            cwd: None,
            env: vec![],
            timeout: None,
            abort: CancellationToken::new(),
            on_stdout: None,
            on_stderr: None,
            max_output_bytes: None,
        };
        // This will be killed by shutdown; the result doesn't matter.
        let _ = env_clone.execute_shell("sleep 30", opts).await;
    });

    // Give the command time to start.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Shutdown must complete in bounded time despite the unbounded command.
    let start = std::time::Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(15), Box::new(sandbox).shutdown()).await;
    let elapsed = start.elapsed();

    assert!(
        result.is_ok(),
        "shutdown must complete within 15s even with an unbounded command"
    );
    assert!(result.unwrap().is_ok(), "shutdown must not return an error");
    assert!(
        elapsed < Duration::from_secs(10),
        "shutdown must be bounded; took {elapsed:?}"
    );

    // The command task should also finish (killed by shutdown).
    let _ = tokio::time::timeout(Duration::from_secs(5), cmd_handle).await;

    // Owned directory must be deleted.
    assert!(
        !work_dir.exists(),
        "owned work_dir must be deleted by shutdown"
    );
}

/// P1: Cancellation while queued behind the lifecycle lock is observed.
///
/// When an operation is queued behind a long-running shell command,
/// cancelling the caller's token must cause the queued operation to
/// return `EnvError::Aborted` promptly, not block until the command
/// finishes.
#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn cancellation_observed_while_queued() {
    if !require_bwrap() {
        return;
    }
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    sandbox.start().await.unwrap();
    let env = sandbox.env.clone();

    // Start a long-running shell command (holds the lock via
    // execute_shell -> acquire_op_lock -> drain_child_output).
    let env_clone = env.clone();
    let cmd_handle = tokio::spawn(async move {
        let opts = ShellOptions {
            cwd: None,
            env: vec![],
            timeout: Some(Duration::from_secs(10)),
            abort: CancellationToken::new(),
            on_stdout: None,
            on_stderr: None,
            max_output_bytes: None,
        };
        let _ = env_clone.execute_shell("sleep 5", opts).await;
    });

    // Give the command time to acquire the lock.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Queue a read operation — it will be blocked behind the lock.
    let abort = CancellationToken::new();
    let abort_clone = abort.clone();
    let env_clone2 = env.clone();
    let read_handle = tokio::spawn(async move {
        env_clone2
            .read_text_file(Path::new("anyfile.txt"), abort_clone)
            .await
    });

    // Cancel the queued read while it's waiting for the lock.
    tokio::time::sleep(Duration::from_millis(50)).await;
    abort.cancel();

    // The queued operation must return Aborted within 2s (well before the
    // 5s sleep finishes).
    let result = tokio::time::timeout(Duration::from_secs(2), read_handle)
        .await
        .expect("cancelled operation must return within 2s")
        .expect("task must not panic");

    assert!(
        matches!(result, Err(EnvError::Aborted)),
        "queued operation must return Aborted after cancellation; got: {:?}",
        result
    );

    // Clean up: let the command finish, then shutdown.
    let _ = tokio::time::timeout(Duration::from_secs(15), cmd_handle).await;
    Box::new(sandbox).shutdown().await.unwrap();
}

// ═══════════════════════════════════════════════════════════════════════════
// Round 7 lifecycle fixes — P0 regression tests
// ═══════════════════════════════════════════════════════════════════════════

/// P0: shutdown() must NOT delete the workspace when lock acquisition times out.
///
/// If an operation holds the lock longer than `SHUTDOWN_KILL_GRACE`,
/// `shutdown()` must return an error and leave the workspace intact rather
/// than deleting it while work may still be in progress.
#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn shutdown_preserves_workspace_on_lock_timeout() {
    if !require_bwrap() {
        return;
    }
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    sandbox.start().await.unwrap();
    let env = sandbox.env.clone(); // Arc<BwrapEnv> — access private fields
    let work_dir = env.working_dir().to_path_buf();

    // Hold the reset_lock in a separate task so shutdown cannot acquire it
    // within the timeout.  The task holds the lock for 8s (> 5s grace).
    let env_for_lock = env.clone();
    let lock_handle = tokio::spawn(async move {
        let guard = env_for_lock.reset_lock.lock().await;
        tokio::time::sleep(Duration::from_secs(8)).await;
        drop(guard);
    });

    // Give the lock-holder time to acquire.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let start = std::time::Instant::now();
    let result = Box::new(sandbox).shutdown().await;
    let elapsed = start.elapsed();

    assert!(
        result.is_err(),
        "shutdown must return Err when lock times out"
    );
    assert!(
        elapsed >= Duration::from_secs(4),
        "shutdown should wait at least ~4s before timing out; took {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(7),
        "shutdown should time out before the lock holder releases; took {elapsed:?}"
    );
    assert!(
        work_dir.exists(),
        "workspace must be preserved when lock acquisition times out"
    );

    // Clean up: wait for the lock holder to release, then delete manually.
    let _ = lock_handle.await;
    let _ = std::fs::remove_dir_all(&work_dir);
}

/// P0: No spawn/register race — a command started concurrently with shutdown
/// must be killed, and shutdown must complete in bounded time.
///
/// This tests the race where shutdown fires before drain_child_output has
/// started polling.  With kill_on_drop(true) and the shutdown token wired
/// into drain, the child is killed regardless of the timing.
#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn shutdown_race_with_command_start() {
    if !require_bwrap() {
        return;
    }
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    sandbox.start().await.unwrap();
    let env = sandbox.env.clone();
    let work_dir = env.working_dir().to_path_buf();

    // Start a long-running command with NO timeout and NO abort.
    let env_clone = env.clone();
    let cmd_handle = tokio::spawn(async move {
        let opts = ShellOptions {
            cwd: None,
            env: vec![],
            timeout: None,
            abort: CancellationToken::new(),
            on_stdout: None,
            on_stderr: None,
            max_output_bytes: None,
        };
        let _ = env_clone.execute_shell("sleep 30", opts).await;
    });

    // Shutdown immediately — no delay to let the command start.
    let start = std::time::Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(15), Box::new(sandbox).shutdown()).await;
    let elapsed = start.elapsed();

    assert!(
        result.is_ok(),
        "shutdown must complete within 15s even when racing with command start"
    );
    assert!(result.unwrap().is_ok(), "shutdown must not return an error");
    assert!(
        elapsed < Duration::from_secs(10),
        "shutdown must be bounded; took {elapsed:?}"
    );

    // The command task must finish (child killed by shutdown).
    let cmd_result = tokio::time::timeout(Duration::from_secs(5), cmd_handle).await;
    assert!(
        cmd_result.is_ok(),
        "command task must complete after shutdown"
    );

    assert!(
        !work_dir.exists(),
        "owned work_dir must be deleted by shutdown"
    );
}

/// P0: Dropping an execute_shell future must kill the child (kill_on_drop).
///
/// If the caller abandons the future, the child process must not survive.
/// We verify by starting a command that creates a sentinel file after a
/// delay, dropping the future, and checking the sentinel never appears.
#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn dropped_future_kills_child() {
    if !require_bwrap() {
        return;
    }
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    sandbox.start().await.unwrap();
    let env = sandbox.env.clone();
    let work_dir = env.working_dir().to_path_buf();

    // Start a command that sleeps then creates a sentinel.
    let env_clone = env.clone();
    let handle = tokio::spawn(async move {
        let opts = ShellOptions {
            cwd: None,
            env: vec![],
            timeout: None,
            abort: CancellationToken::new(),
            on_stdout: None,
            on_stderr: None,
            max_output_bytes: None,
        };
        let _ = env_clone
            .execute_shell("sleep 3 && touch sentinel.txt", opts)
            .await;
    });

    // Give the command time to start and acquire the lock.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Abort the task — this drops the execute_shell future.
    handle.abort();

    // Wait long enough that the sleep would have completed if the child
    // were still alive (3s sleep + margin).
    tokio::time::sleep(Duration::from_secs(4)).await;

    assert!(
        !work_dir.join("sentinel.txt").exists(),
        "child must have been killed when future was dropped — sentinel must not exist"
    );

    // Sandbox should still be usable.
    let result = env.execute_shell("echo alive", shell_opts()).await;
    assert!(
        result.is_ok(),
        "sandbox should still be usable after dropped future"
    );

    Box::new(sandbox).shutdown().await.unwrap();
}

/// P0: reset() must interrupt a running command (no timeout) via token swap.
///
/// reset() swaps in a fresh shutdown token and cancels the old one.
/// drain_child_output observes the cancellation, kills+waits the child,
/// and reset acquires the lock in bounded time.
#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn reset_interrupts_running_command() {
    if !require_bwrap() {
        return;
    }
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    sandbox.start().await.unwrap();
    let env = sandbox.env.clone();

    // Start a long-running command with NO timeout and NO abort.
    let env_clone = env.clone();
    let cmd_handle = tokio::spawn(async move {
        let opts = ShellOptions {
            cwd: None,
            env: vec![],
            timeout: None,
            abort: CancellationToken::new(),
            on_stdout: None,
            on_stderr: None,
            max_output_bytes: None,
        };
        let _ = env_clone.execute_shell("sleep 30", opts).await;
    });

    // Give the command time to start.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Reset must complete in bounded time despite the running command.
    let start = std::time::Instant::now();
    sandbox.reset().await.unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "reset must complete quickly despite running command; took {elapsed:?}"
    );

    // The command task must finish (killed by reset's token swap).
    let cmd_result = tokio::time::timeout(Duration::from_secs(5), cmd_handle).await;
    assert!(cmd_result.is_ok(), "command task must complete after reset");

    // Sandbox should still be usable after reset (fresh token installed).
    let result = env.execute_shell("echo alive", shell_opts()).await;
    assert!(result.is_ok(), "sandbox should still be usable after reset");

    Box::new(sandbox).shutdown().await.unwrap();
}

/// P0: No orphan bwrap processes remain after shutdown kills a running command.
///
/// Instead of counting global bwrap processes (which is unreliable on shared
/// hosts with parallel tests), this test uses a sentinel file: the command
/// creates a marker file after a delay.  If shutdown kills the child properly,
/// the sentinel never appears.  If the child survives (orphan), the sentinel
/// appears.
#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn no_orphan_after_shutdown() {
    if !require_bwrap() {
        return;
    }
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    sandbox.start().await.unwrap();
    let env = sandbox.env.clone();
    let work_dir = env.working_dir().to_path_buf();

    // Start a command that sleeps then creates a sentinel file.  If the child
    // is killed within the sleep, the sentinel never appears.
    let env_clone = env.clone();
    let cmd_handle = tokio::spawn(async move {
        let opts = ShellOptions {
            cwd: None,
            env: vec![],
            timeout: None,
            abort: CancellationToken::new(),
            on_stdout: None,
            on_stderr: None,
            max_output_bytes: None,
        };
        let _ = env_clone
            .execute_shell("sleep 3 && touch sentinel_survivor.txt", opts)
            .await;
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Shutdown must complete in bounded time.
    let start = std::time::Instant::now();
    Box::new(sandbox).shutdown().await.unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "shutdown must be bounded; took {elapsed:?}"
    );

    // Command task must finish.
    let _ = tokio::time::timeout(Duration::from_secs(5), cmd_handle).await;

    // Wait long enough that the sleep would have completed if the child
    // were still alive (3s sleep + margin).
    tokio::time::sleep(Duration::from_secs(4)).await;

    assert!(
        !work_dir.join("sentinel_survivor.txt").exists(),
        "child must have been killed by shutdown — sentinel must not exist"
    );
}
// ═══════════════════════════════════════════════════════════════════════════
// Round 8 fixes — epoch linearization + guaranteed reap on drop
// ═══════════════════════════════════════════════════════════════════════════

/// P1: reset() token swap must linearize with commands that have already
/// acquired the op-lock.
///
/// A command that passed acquire_op_lock() holds a *snapshot* of the shutdown
/// token at lock-acquisition time.  If reset() swaps the token afterwards,
/// the command's snapshot is the OLD token — the one reset() cancels — so the
/// command still observes the cancellation and is interrupted.
///
/// This test verifies that reset completes in bounded time even when a
/// command is actively running (holding the lock via drain_child_output).
/// If the epoch were not linearized, the command would hold the new
/// (uncancelled) token and never exit, causing reset to time out.
#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn reset_linearizes_with_running_command() {
    if !require_bwrap() {
        return;
    }
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    sandbox.start().await.unwrap();
    let env = sandbox.env.clone();

    // Start a long-running command (no timeout, no abort).
    let env_clone = env.clone();
    let cmd_handle = tokio::spawn(async move {
        let opts = ShellOptions {
            cwd: None,
            env: vec![],
            timeout: None,
            abort: CancellationToken::new(),
            on_stdout: None,
            on_stderr: None,
            max_output_bytes: None,
        };
        let _ = env_clone.execute_shell("sleep 60", opts).await;
    });

    // Give the command time to acquire the lock and start the child.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // reset() must complete in bounded time.  It swaps the token and cancels
    // the old one.  The running command's drain_child_output_guarded observes
    // the old token's cancellation, kills+waits the child, and releases the
    // lock so reset can proceed.
    let start = std::time::Instant::now();
    sandbox.reset().await.expect("reset must succeed");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "reset must complete in bounded time; took {elapsed:?}"
    );

    // The command must have been interrupted.
    let cmd_result = tokio::time::timeout(Duration::from_secs(5), cmd_handle).await;
    assert!(cmd_result.is_ok(), "command task must complete after reset");

    // Sandbox should still be usable (fresh token installed by reset).
    let result = env.execute_shell("echo alive", shell_opts()).await;
    assert!(result.is_ok(), "sandbox should still be usable after reset");

    Box::new(sandbox).shutdown().await.unwrap();
}

/// P1: Dropping an execute_shell future must guarantee the child is killed,
/// not just left as an orphan.
///
/// When the future is dropped, the ChildLease's Drop delegates to the
/// tokio::spawn task that calls start_kill() + wait(), ensuring the child
/// is killed and reaped.  This test verifies the child does not survive
/// long enough to create a sentinel file.
#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn dropped_future_guarantees_reap() {
    if !require_bwrap() {
        return;
    }
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    sandbox.start().await.unwrap();
    let env = sandbox.env.clone();
    let work_dir = env.working_dir().to_path_buf();

    // Start a command that sleeps then creates a sentinel file.
    let env_clone = env.clone();
    let handle = tokio::spawn(async move {
        let opts = ShellOptions {
            cwd: None,
            env: vec![],
            timeout: None,
            abort: CancellationToken::new(),
            on_stdout: None,
            on_stderr: None,
            max_output_bytes: None,
        };
        let _ = env_clone
            .execute_shell("sleep 3 && touch sentinel_dropped.txt", opts)
            .await;
    });

    // Give the command time to start.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Abort the task — this drops the execute_shell future, which drops the
    // drain_child_output_guarded future, which drops the ChildLease.
    handle.abort();

    // Wait long enough that the sleep would have completed if the child
    // were still alive (3s sleep + margin).
    tokio::time::sleep(Duration::from_secs(4)).await;

    assert!(
        !work_dir.join("sentinel_dropped.txt").exists(),
        "child must have been killed when future was dropped — sentinel must not exist"
    );

    // Sandbox should still be usable.
    let result = env.execute_shell("echo alive", shell_opts()).await;
    assert!(
        result.is_ok(),
        "sandbox should still be usable after dropped future"
    );

    Box::new(sandbox).shutdown().await.unwrap();
}

/// P1: reset() must close admission atomically before token swap.
///
/// Without the Resetting state, a new operation can see Running, snapshot
/// the fresh token, acquire the lock, and start a long command — causing
/// reset to time out.  With Resetting, new operations are rejected during
/// the entire reset window.
///
/// This test holds the reset_lock manually so reset blocks in the lock-
/// acquisition phase.  While reset is blocked (state is Resetting), a new
/// command is attempted and must be rejected.
#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn reset_closes_admission_atomically() {
    if !require_bwrap() {
        return;
    }
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    sandbox.start().await.unwrap();
    let env = sandbox.env.clone();

    // Hold the reset_lock so reset() blocks in the lock-acquisition phase.
    // This gives us a deterministic window where the lifecycle is Resetting.
    let lock_guard = env.reset_lock.lock().await;

    // Start reset() — it will:
    // 1. begin_reset() -> state = Resetting
    // 2. swap token, cancel old
    // 3. try to acquire reset_lock -> blocks (we hold it)
    //
    // We spawn reset in a task so it doesn't borrow `sandbox` directly.
    // The task owns the sandbox and returns it so we can shut it down
    // after the test — otherwise the auto-generated /tmp/sandbox-bwrap-*
    // workspace would leak.
    let reset_handle = tokio::spawn(async move {
        let start = std::time::Instant::now();
        sandbox.reset().await.expect("reset must succeed");
        (sandbox, start.elapsed())
    });

    // Give reset time to call begin_reset() and reach the lock acquisition.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // State is now Resetting.  A new command must be rejected immediately
    // by acquire_op_lock (lifecycle != Running), without trying to acquire
    // the lock.
    let new_cmd_result = env
        .execute_shell("echo should_be_rejected", shell_opts())
        .await;
    assert!(
        new_cmd_result.is_err(),
        "new command during reset must be rejected (lifecycle is Resetting); got: Ok"
    );

    // Release the lock so reset can proceed.
    drop(lock_guard);

    // Reset must complete successfully.
    let (sandbox, reset_elapsed) = reset_handle.await.expect("reset task must not panic");
    assert!(
        reset_elapsed < Duration::from_secs(5),
        "reset must complete quickly after lock release; took {reset_elapsed:?}"
    );

    // Sandbox should be usable after reset (state is Running again).
    let result = sandbox.env.execute_shell("echo alive", shell_opts()).await;
    assert!(result.is_ok(), "sandbox should be usable after reset");

    // Clean up the auto-generated workspace.
    Box::new(sandbox).shutdown().await.unwrap();
}

/// P0: Dropping the reset() future must roll back the lifecycle to Running.
///
/// Without the RAII ResetLifecycleGuard, a dropped reset future leaves the
/// lifecycle permanently stuck in Resetting — all subsequent operations fail.
/// This test holds the reset_lock, polls reset briefly (it enters Resetting
/// and blocks on the lock), then drops the future.  The lifecycle must
/// return to Running so subsequent commands succeed.
#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn reset_future_drop_rolls_back_to_running() {
    if !require_bwrap() {
        return;
    }
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    sandbox.start().await.unwrap();
    let env = sandbox.env.clone();

    // Hold the lock so reset() blocks during lock acquisition.
    let lock_guard = env.reset_lock.lock().await;

    // Start reset — poll it briefly so it enters Resetting and blocks on
    // the lock.  Then drop the future (simulating cancellation).
    {
        let reset_future = sandbox.reset();
        tokio::pin!(reset_future);
        let _ = tokio::time::timeout(Duration::from_millis(200), &mut reset_future).await;
        // reset_future is dropped here — the RAII guard must roll back
        // to Running.
    }

    // Release the lock — no longer needed.
    drop(lock_guard);
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Lifecycle must be back to Running — the sandbox is usable.
    assert_eq!(
        env.lifecycle(),
        super::LifecycleState::Running,
        "lifecycle must roll back to Running after reset future is dropped"
    );

    // Verify the sandbox is actually usable.
    let result = env.execute_shell("echo alive", shell_opts()).await;
    assert!(
        result.is_ok(),
        "sandbox must be usable after reset future drop"
    );

    // Clean up the auto-generated workspace.
    Box::new(sandbox).shutdown().await.unwrap();
}

// ═══════════════════════════════════════════════════════════════════════════
// Phase-controlled reset cancellation tests (#97 P0)
// ═══════════════════════════════════════════════════════════════════════════

/// Helper: create a BwrapSandbox with a cleanup hook installed.
fn sandbox_with_hook(
    mode: super::CleanupMode,
) -> (
    BwrapSandbox,
    std::sync::Arc<super::BwrapEnv>,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    let env = sandbox.env.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (gate_tx, gate_rx) = std::sync::mpsc::channel();
    env.set_cleanup_hook(super::CleanupHook {
        started: started_tx,
        gate: std::sync::Mutex::new(gate_rx),
        mode,
    });
    (sandbox, env, started_rx, gate_tx)
}

/// P0: Dropping the reset future **after mutation has started** must NOT
/// reopen admission.  The blocking cleanup task continues holding the lock
/// and lifecycle ownership; new operations are rejected until cleanup
/// completes.
///
/// This is the core race fixed by moving `OwnedMutexGuard` and lifecycle
/// transition into the `spawn_blocking` task.  Before the fix, dropping the
/// future released the borrowed `MutexGuard` and rolled back to `Running`,
/// allowing new writes that the detached cleanup would then delete.
#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn reset_cancelled_during_mutation_rejects_new_work() {
    if !require_bwrap() {
        return;
    }
    let (sandbox, env, started_rx, gate_tx) = sandbox_with_hook(super::CleanupMode::Normal);
    sandbox.start().await.unwrap();
    let work_dir = env.working_dir().to_path_buf();

    // Write a file before reset so cleanup has something to delete.
    env.write_file(
        std::path::Path::new("before.txt"),
        b"data",
        CancellationToken::new(),
    )
    .await
    .unwrap();

    // Start reset.  The cleanup task will signal `started`, then block on
    // `gate`.  We use select! to cancel the reset future once cleanup has
    // started — this drops the future while spawn_blocking is still running.
    let reset_fut = sandbox.reset();
    let started_fut = tokio::task::spawn_blocking(move || {
        started_rx.recv_timeout(Duration::from_secs(5)).is_ok()
    });

    tokio::select! {
        _ = started_fut => {
            // Cleanup started — reset_fut will be dropped by select!.
        }
        result = reset_fut => {
            panic!("reset must not complete before gate signal; got: {:?}", result);
        }
    }
    // reset_fut is dropped here.  The spawn_blocking task continues,
    // holding the OwnedMutexGuard and TaintOnDrop guard.

    // Lifecycle must NOT be Running — cleanup is still in progress.
    assert_ne!(
        env.lifecycle(),
        super::LifecycleState::Running,
        "lifecycle must not be Running while cleanup is still running"
    );

    // New write operations must be rejected (lifecycle is Resetting,
    // lock is held by the blocking task).
    let write_result = env
        .write_file(
            std::path::Path::new("admitted-after-cancel.txt"),
            b"should-not-succeed",
            CancellationToken::new(),
        )
        .await;
    assert!(
        write_result.is_err(),
        "write must be rejected while cleanup is in progress"
    );

    // Let cleanup proceed.
    gate_tx.send(()).unwrap();

    // Wait for cleanup to finish and lifecycle to transition to Running.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if env.lifecycle() == super::LifecycleState::Running {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("timeout waiting for lifecycle to return to Running");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The file we tried to write during cleanup must not exist — the write
    // was rejected, so the detached cleanup could not have deleted it.
    assert!(
        !work_dir.join("admitted-after-cancel.txt").exists(),
        "file written during cleanup must not exist (write was rejected)"
    );

    // The pre-existing file must have been deleted by cleanup.
    assert!(
        !work_dir.join("before.txt").exists(),
        "pre-existing file must be deleted by cleanup"
    );

    Box::new(sandbox).shutdown().await.unwrap();
}

/// P0: If cleanup returns an I/O error after partial deletion, the sandbox
/// must transition to `Tainted` — not roll back to `Running`.  A partially
/// mutated workspace cannot be trusted.
#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn reset_cleanup_error_transitions_to_tainted() {
    if !require_bwrap() {
        return;
    }
    let (sandbox, env, _started_rx, gate_tx) = sandbox_with_hook(super::CleanupMode::Fail);
    sandbox.start().await.unwrap();

    env.write_file(
        std::path::Path::new("file.txt"),
        b"data",
        CancellationToken::new(),
    )
    .await
    .unwrap();

    // Pre-send gate so the hook doesn't block.
    gate_tx.send(()).unwrap();

    let result = sandbox.reset().await;
    assert!(
        result.is_err(),
        "reset must return error on cleanup failure"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("injected cleanup failure"),
        "error must mention injected failure; got: {err_msg}"
    );

    // Lifecycle must be Tainted, not Running.
    assert_eq!(
        env.lifecycle(),
        super::LifecycleState::Tainted,
        "lifecycle must be Tainted after cleanup error"
    );

    // New operations must be rejected.
    let write_result = env
        .write_file(
            std::path::Path::new("post.txt"),
            b"x",
            CancellationToken::new(),
        )
        .await;
    assert!(
        write_result.is_err(),
        "write must be rejected in Tainted state"
    );

    // Further reset attempts must also fail.
    let reset_result = sandbox.reset().await;
    assert!(
        reset_result.is_err(),
        "reset must fail on a Tainted sandbox"
    );

    // P1: Public lifecycle must reflect Tainted — is_running() returns
    // false and start() refuses to restart a tainted sandbox.
    assert!(
        !sandbox.is_running(),
        "is_running() must return false when sandbox is Tainted"
    );
    let start_result = sandbox.start().await;
    assert!(
        start_result.is_err(),
        "start() must fail on a Tainted sandbox"
    );

    Box::new(sandbox).shutdown().await.unwrap();
}

/// P0: If the cleanup task panics, the sandbox must transition to `Tainted`.
/// The `TaintOnDrop` guard ensures the transition even when neither
/// `finish_reset` nor `taint` was explicitly called.
#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn reset_cleanup_panic_transitions_to_tainted() {
    if !require_bwrap() {
        return;
    }
    let (sandbox, env, _started_rx, gate_tx) = sandbox_with_hook(super::CleanupMode::Panic);
    sandbox.start().await.unwrap();

    env.write_file(
        std::path::Path::new("file.txt"),
        b"data",
        CancellationToken::new(),
    )
    .await
    .unwrap();

    gate_tx.send(()).unwrap();

    let result = sandbox.reset().await;
    assert!(result.is_err(), "reset must return error on cleanup panic");

    // Lifecycle must be Tainted — the TaintOnDrop guard transitioned it.
    assert_eq!(
        env.lifecycle(),
        super::LifecycleState::Tainted,
        "lifecycle must be Tainted after cleanup panic"
    );

    // New operations must be rejected.
    let write_result = env
        .write_file(
            std::path::Path::new("post.txt"),
            b"x",
            CancellationToken::new(),
        )
        .await;
    assert!(
        write_result.is_err(),
        "write must be rejected in Tainted state"
    );

    // P1: is_running() must return false for a Tainted sandbox.
    assert!(
        !sandbox.is_running(),
        "is_running() must return false when sandbox is Tainted (panic path)"
    );

    Box::new(sandbox).shutdown().await.unwrap();
}

/// P0: If cleanup fails AFTER partial deletion (some files deleted, some
/// remaining), the sandbox must still transition to `Tainted`.  This
/// covers the case where the workspace is partially mutated.
#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn reset_partial_cleanup_error_transitions_to_tainted() {
    if !require_bwrap() {
        return;
    }
    let (sandbox, env, _started_rx, gate_tx) = sandbox_with_hook(super::CleanupMode::FailPartial);
    sandbox.start().await.unwrap();
    let work_dir = env.working_dir().to_path_buf();

    // Write multiple files so cleanup has something to partially delete.
    env.write_file(
        std::path::Path::new("file_a.txt"),
        b"data_a",
        CancellationToken::new(),
    )
    .await
    .unwrap();
    env.write_file(
        std::path::Path::new("file_b.txt"),
        b"data_b",
        CancellationToken::new(),
    )
    .await
    .unwrap();

    // Pre-send gate so the hook doesn't block.
    gate_tx.send(()).unwrap();

    let result = sandbox.reset().await;
    assert!(
        result.is_err(),
        "reset must return error on partial cleanup failure"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("partial cleanup failure"),
        "error must mention partial failure; got: {err_msg}"
    );

    // Lifecycle must be Tainted — partial mutation makes the workspace
    // untrustworthy.
    assert_eq!(
        env.lifecycle(),
        super::LifecycleState::Tainted,
        "lifecycle must be Tainted after partial cleanup error"
    );

    // At least one file must have been deleted (proving partial cleanup
    // actually happened).  The order of directory iteration is not
    // guaranteed, so we check that not all files remain.
    let a_exists = work_dir.join("file_a.txt").exists();
    let b_exists = work_dir.join("file_b.txt").exists();
    assert!(
        !(a_exists && b_exists),
        "at least one file must have been deleted before the failure"
    );

    // is_running() must return false for a Tainted sandbox.
    assert!(
        !sandbox.is_running(),
        "is_running() must return false when sandbox is Tainted (partial cleanup)"
    );

    Box::new(sandbox).shutdown().await.unwrap();
}
