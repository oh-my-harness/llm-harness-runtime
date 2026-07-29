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
// Reset safety — no bwrap required
// ═══════════════════════════════════════════════════════════════════════════

/// Workspace reset only affects the work dir, not external paths.
#[tokio::test]
async fn reset_only_affects_work_dir() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let outside_file = dir.path().join("outside.txt");
    tokio::fs::write(&outside_file, "should_survive")
        .await
        .unwrap();

    tokio::fs::write(work_dir.join("inside.txt"), "should_be_deleted")
        .await
        .unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir.clone())).unwrap();
    sandbox.reset().await.unwrap();

    let entries: Vec<_> = std::fs::read_dir(&work_dir).unwrap().collect();
    assert!(entries.is_empty(), "work dir must be empty after reset");

    let content = tokio::fs::read_to_string(&outside_file).await.unwrap();
    assert_eq!(content, "should_survive");
}

/// P0: reset() refuses root path.
#[tokio::test]
async fn reset_refuses_unsafe_paths() {
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

/// P0: reset() refuses `/tmp` (shallow path, would delete all temp files).
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
    assert!(result.is_err(), "reset must refuse /tmp (shallow path)");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("safe disposable root"),
        "error must mention disposable root"
    );
}

/// P0: reset() refuses `.` (canonicalizes to cwd).
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
    assert!(
        result.is_err(),
        "reset must refuse '.' (canonicalizes to cwd)"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
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
