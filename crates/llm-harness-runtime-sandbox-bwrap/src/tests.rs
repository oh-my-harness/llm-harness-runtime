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

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use llm_harness_runtime::platform::sandbox::{NetRule, ResourceLimits, SandboxConfig};
use llm_harness_types::ShellOptions;
use tokio_util::sync::CancellationToken;

use super::*;

fn default_config(work_dir: PathBuf) -> SandboxConfig {
    SandboxConfig {
        fs_allowlist: vec![],
        fs_denylist: vec![],
        net_allowlist: vec![],
        resource_limits: ResourceLimits {
            max_cpus: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: Some(work_dir),
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
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: None,
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
    }
}

/// Returns true if bwrap is installed and can invoke a sandbox.
fn bwrap_available() -> bool {
    std::process::Command::new("bwrap")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Asserts that `bwrap` is installed; panics otherwise.
///
/// Tests that need bwrap are marked `#[ignore]` so a default `cargo test`
/// reports them as ignored. When run explicitly with `--include-ignored`,
/// this guard fails loudly if bwrap is absent.
fn require_bwrap() {
    if !bwrap_available() {
        panic!("bwrap is required for this test (install bubblewrap)");
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Config validation (fail-closed) — no bwrap required
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn config_with_fs_denylist_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let config = SandboxConfig {
        fs_allowlist: vec![],
        fs_denylist: vec![PathBuf::from("/secret")],
        net_allowlist: vec![],
        resource_limits: ResourceLimits {
            max_cpus: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: Some(dir.path().to_path_buf()),
    };
    let result = BwrapSandbox::new(config);
    assert!(result.is_err(), "fs_denylist must be rejected");
    assert!(result.unwrap_err().to_string().contains("fs_denylist"));
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
fn config_with_resource_limits_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let config = SandboxConfig {
        fs_allowlist: vec![],
        fs_denylist: vec![],
        net_allowlist: vec![],
        resource_limits: ResourceLimits {
            max_cpus: Some(2),
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: Some(dir.path().to_path_buf()),
    };
    let result = BwrapSandbox::new(config);
    assert!(result.is_err(), "max_cpus must be rejected");
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
    let env = sandbox.env();

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
    let env = sandbox.env();

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
    let env = sandbox.env();

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
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: Some(work_dir),
    };
    let sandbox = BwrapSandbox::new(config).unwrap();
    let env = sandbox.env();

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
    let env = sandbox.env();

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
    let env = sandbox.env();

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
    let env = sandbox.env();

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
    let env = sandbox.env();
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
    let env = sandbox.env();
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
    let env = sandbox.env();
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
    require_bwrap();
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
    require_bwrap();
    let dir = tempfile::tempdir().unwrap();
    let secret_path = dir.path().join("host-secret.txt");
    tokio::fs::write(&secret_path, "host_secret_value")
        .await
        .unwrap();

    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir.clone())).unwrap();
    sandbox.start().await.unwrap();

    let env = sandbox.env();
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
    };
    let result = env.execute_shell(&cmd, opts).await.unwrap();
    assert!(
        result.stdout.contains("BLOCKED"),
        "sandbox must prevent reading host files; got: {}",
        result.stdout
    );
}

#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn shell_cannot_read_etc_passwd() {
    require_bwrap();
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir)).unwrap();
    sandbox.start().await.unwrap();

    let env = sandbox.env();
    let cmd = "cat /etc/passwd 2>/dev/null && echo LEAKED || echo BLOCKED";
    let opts = ShellOptions {
        cwd: None,
        env: vec![],
        timeout: Some(Duration::from_secs(10)),
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
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
    require_bwrap();
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();
    tokio::fs::write(work_dir.join("readable.txt"), "workspace_content")
        .await
        .unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir.clone())).unwrap();
    sandbox.start().await.unwrap();

    let env = sandbox.env();
    let result = env
        .execute_shell("cat readable.txt", shell_opts())
        .await
        .unwrap();
    assert_eq!(result.stdout.trim(), "workspace_content");
}

#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn empty_net_allowlist_blocks_network() {
    require_bwrap();
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir)).unwrap();
    sandbox.start().await.unwrap();

    let env = sandbox.env();
    let cmd = "timeout 3 bash -c 'echo test > /dev/tcp/8.8.8.8/53' 2>&1 && echo NETWORK_ALLOWED || echo NETWORK_BLOCKED";
    let opts = ShellOptions {
        cwd: None,
        env: vec![],
        timeout: Some(Duration::from_secs(15)),
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
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
    require_bwrap();
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir)).unwrap();
    sandbox.start().await.unwrap();

    let env = sandbox.env();
    let opts = ShellOptions {
        cwd: None,
        env: vec![("SANDBOX_INJECTED", "injected_value")],
        timeout: Some(Duration::from_secs(10)),
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
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
    require_bwrap();
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir)).unwrap();
    sandbox.start().await.unwrap();

    let env = sandbox.env();
    let opts = ShellOptions {
        cwd: None,
        env: vec![("MY_CUSTOM_VAR", "custom_value_42")],
        timeout: Some(Duration::from_secs(10)),
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
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
    require_bwrap();
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir.clone())).unwrap();
    sandbox.start().await.unwrap();

    let env = sandbox.env();
    let cmd = "(sleep 3 && echo SURVIVED > sentinel.txt) & sleep 30";
    let opts = ShellOptions {
        cwd: None,
        env: vec![],
        timeout: Some(Duration::from_secs(2)),
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
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
    require_bwrap();
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir)).unwrap();
    sandbox.start().await.unwrap();

    let env = sandbox.env();
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
    require_bwrap();
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let config = SandboxConfig {
        fs_allowlist: vec![],
        fs_denylist: vec![],
        net_allowlist: vec![],
        resource_limits: ResourceLimits {
            max_cpus: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: Some(Duration::from_secs(1)),
        },
        work_dir: Some(work_dir),
    };
    let sandbox = BwrapSandbox::new(config).unwrap();
    sandbox.start().await.unwrap();
    let env = sandbox.env();

    // No per-call timeout; config timeout (1s) should apply.
    let opts = ShellOptions {
        cwd: None,
        env: vec![],
        timeout: None,
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
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
    let env = sandbox.env();

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
    require_bwrap();
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    sandbox.start().await.unwrap();
    let env = sandbox.env();
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
    require_bwrap();
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    sandbox.start().await.unwrap();
    let env = sandbox.env();

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
    require_bwrap();
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
    require_bwrap();
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    sandbox.start().await.unwrap();
    let env = sandbox.env();
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
    require_bwrap();
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    sandbox.start().await.unwrap();
    let env = sandbox.env();
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
    require_bwrap();
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    sandbox.start().await.unwrap();
    let env = sandbox.env();

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
/// Verifies the full kill+wait path: shutdown cancels the token,
/// drain_child_output kills and reaps the child, and no zombie or orphan
/// survives.
#[tokio::test]
#[ignore = "requires bwrap; run with --include-ignored"]
async fn no_orphan_after_shutdown() {
    require_bwrap();
    let sandbox = BwrapSandbox::new(owned_config()).unwrap();
    sandbox.start().await.unwrap();
    let env = sandbox.env();

    // Start a long-running command.
    let env_clone = env.clone();
    let cmd_handle = tokio::spawn(async move {
        let opts = ShellOptions {
            cwd: None,
            env: vec![],
            timeout: None,
            abort: CancellationToken::new(),
            on_stdout: None,
            on_stderr: None,
        };
        let _ = env_clone.execute_shell("sleep 30", opts).await;
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Count bwrap processes before shutdown.
    let before = count_bwrap_processes();

    Box::new(sandbox).shutdown().await.unwrap();

    // Command task must finish.
    let _ = tokio::time::timeout(Duration::from_secs(5), cmd_handle).await;

    // Give the OS a moment to reap any remaining zombies.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let after = count_bwrap_processes();
    assert!(
        after <= before,
        "no new bwrap processes should remain after shutdown; before={before}, after={after}"
    );
}

/// Helper: count bwrap processes on the system.
fn count_bwrap_processes() -> usize {
    std::process::Command::new("pgrep")
        .arg("-c")
        .arg("bwrap")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<usize>()
                .unwrap_or(0)
        })
        .unwrap_or(0)
}
