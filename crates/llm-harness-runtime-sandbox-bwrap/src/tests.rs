//! Integration tests for BwrapSandbox isolation.
//!
//! These tests run **real commands** through `sandbox.env().execute_shell()`
//! and real file operations through `sandbox.env()`.
//!
//! When bwrap is not installed, tests that require it are marked as
//! **ignored** (not silently passed) so CI reports them accurately.

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

/// Returns true if bwrap is available.
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
/// reports them as ignored rather than falsely passed. When run explicitly
/// with `--include-ignored`, this guard fails loudly if bwrap is absent
/// instead of silently passing.
fn require_bwrap() {
    if !bwrap_available() {
        panic!("bwrap is required for this test (install bubblewrap)");
    }
}

// ── Lifecycle ──────────────────────────────────────────────────────────────

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

#[tokio::test]
#[ignore = "requires bwrap to be absent; run with --include-ignored on a host without bwrap"]
async fn bwrap_sandbox_start_fails_when_unavailable() {
    if bwrap_available() {
        eprintln!("ignored: bwrap is installed, cannot test failure path");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sandbox = BwrapSandbox::new(default_config(dir.path().to_path_buf())).unwrap();
    let result = sandbox.start().await;
    assert!(result.is_err());
}

// ── Config validation (fail-closed) ────────────────────────────────────────

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

// ── Filesystem isolation: file API path restrictions ───────────────────────

/// Acceptance: File API rejects absolute paths outside the workspace.
#[tokio::test]
async fn file_api_rejects_absolute_path_outside_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir)).unwrap();
    let env = sandbox.env();

    let result = env
        .read_text_file(
            Path::new("/etc/passwd"),
            CancellationToken::new(),
        )
        .await;
    assert!(
        result.is_err(),
        "reading /etc/passwd via file API must be rejected"
    );
    let err = result.unwrap_err();
    assert!(
        matches!(err, EnvError::PermissionDenied(_)),
        "expected PermissionDenied, got {err:?}"
    );
}

/// Acceptance: File API rejects `..` traversal outside the workspace.
#[tokio::test]
async fn file_api_rejects_dotdot_traversal() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();
    tokio::fs::write(dir.path().join("outside.txt"), "host_data")
        .await
        .unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir)).unwrap();
    let env = sandbox.env();

    let result = env
        .read_text_file(
            Path::new("../outside.txt"),
            CancellationToken::new(),
        )
        .await;
    assert!(
        result.is_err(),
        "../ traversal must be rejected by file API"
    );
    assert!(matches!(
        result.unwrap_err(),
        EnvError::PermissionDenied(_)
    ));
}

/// Acceptance: File API allows paths inside the workspace.
#[tokio::test]
async fn file_api_allows_paths_inside_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();
    tokio::fs::write(work_dir.join("inside.txt"), "workspace_data")
        .await
        .unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir)).unwrap();
    let env = sandbox.env();

    let content = env
        .read_text_file(Path::new("inside.txt"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(content, "workspace_data");
}

/// Acceptance: File API allows paths in fs_allowlist.
#[tokio::test]
async fn file_api_allows_fs_allowlist_paths() {
    let dir = tempfile::tempdir().unwrap();
    let shared_dir = dir.path().join("shared");
    tokio::fs::create_dir_all(&shared_dir).await.unwrap();
    tokio::fs::write(shared_dir.join("shared.txt"), "shared_data")
        .await
        .unwrap();

    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

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

    let content = env
        .read_text_file(&shared_dir.join("shared.txt"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(content, "shared_data");
}

// ── Filesystem isolation: shell command restrictions ───────────────────────

/// Acceptance: A shell command cannot read a host file outside the allowlist.
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

/// Acceptance: /etc/passwd is not readable from inside the sandbox.
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

/// Acceptance: The workspace is accessible from inside the sandbox.
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

// ── Network isolation ──────────────────────────────────────────────────────

/// Acceptance: Empty network allowlist prevents network access.
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

// ── Environment credential isolation ───────────────────────────────────────

/// Acceptance: Host environment variables are not inherited.
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

    // Host env vars must NOT leak.
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

// ── Timeout kills process tree (with sentinel) ─────────────────────────────

/// Acceptance: Timeout terminates the entire process tree, including
/// background children. Verified via a sentinel file that a background child
/// would write if it survived.
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
    // Spawn a background child that writes a sentinel after 3 seconds.
    // If timeout (2s) kills the parent but not the child, the sentinel
    // will appear.
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

    // Wait beyond the child's planned write time to confirm it was killed.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let sentinel = work_dir.join("sentinel.txt");
    assert!(
        !sentinel.exists(),
        "background child must have been killed — sentinel file must not exist"
    );
}

// ── Abort cancels running command ──────────────────────────────────────────

/// Acceptance: Abort terminates the running command.
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

// ── Workspace reset safety ─────────────────────────────────────────────────

/// Acceptance: Workspace reset only affects the work dir, not external paths.
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

    let entries: Vec<_> = std::fs::read_dir(&work_dir)
        .unwrap()
        .collect();
    assert!(entries.is_empty(), "work dir must be empty after reset");

    let content = tokio::fs::read_to_string(&outside_file)
        .await
        .unwrap();
    assert_eq!(content, "should_survive");
}

/// Acceptance: reset() refuses root or unsafe paths.
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
    assert!(
        result.is_err(),
        "reset must refuse root path"
    );
}

// ── Injected env vars are visible ──────────────────────────────────────────

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
    let result = env.execute_shell("echo $MY_CUSTOM_VAR", opts).await.unwrap();
    assert_eq!(result.stdout.trim(), "custom_value_42");
}
