//! Integration tests for BwrapSandbox isolation.
//!
//! These tests run **real commands** through `sandbox.env().execute_shell()`,
//! not just argument construction. They are guarded by a runtime `bwrap`
//! availability check so they skip gracefully on systems without bubblewrap.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use llm_harness_runtime::platform::sandbox::{ResourceLimits, SandboxConfig};
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

/// Returns true if bwrap is available; tests use this to skip.
fn bwrap_available() -> bool {
    std::process::Command::new("bwrap")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ── Lifecycle ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn bwrap_sandbox_starts_when_available() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sandbox = BwrapSandbox::new(default_config(dir.path().to_path_buf()));
    sandbox.start().await.unwrap();
    assert!(sandbox.is_running());
    Box::new(sandbox).shutdown().await.unwrap();
}

#[tokio::test]
async fn bwrap_sandbox_start_fails_when_unavailable() {
    if bwrap_available() {
        eprintln!("skipping: bwrap is installed, cannot test failure path");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sandbox = BwrapSandbox::new(default_config(dir.path().to_path_buf()));
    let result = sandbox.start().await;
    assert!(result.is_err());
}

// ── Filesystem isolation ───────────────────────────────────────────────────

/// Acceptance: A shell command cannot read a host file outside the allowlist.
#[tokio::test]
async fn shell_cannot_read_host_file_outside_allowlist() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let secret_path = dir.path().join("host-secret.txt");
    tokio::fs::write(&secret_path, "host_secret_value")
        .await
        .unwrap();

    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir.clone()));
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
        "sandbox must prevent reading host files outside the allowlist; got: {}",
        result.stdout
    );
    assert!(
        !result.stdout.contains("host_secret_value"),
        "host secret must not leak into sandbox"
    );
}

/// Acceptance: The workspace is accessible from inside the sandbox.
#[tokio::test]
async fn workspace_files_are_accessible_from_sandbox() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();
    tokio::fs::write(work_dir.join("readable.txt"), "workspace_content")
        .await
        .unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir.clone()));
    sandbox.start().await.unwrap();

    let env = sandbox.env();
    let cmd = "cat readable.txt";
    let result = env
        .execute_shell(cmd, shell_opts())
        .await
        .unwrap();
    assert_eq!(result.stdout.trim(), "workspace_content");
}

// ── Network isolation ──────────────────────────────────────────────────────

/// Acceptance: Empty network allowlist prevents network access.
#[tokio::test]
async fn empty_net_allowlist_blocks_network() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir));
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

/// Acceptance: Environment credentials are not inherited unless explicitly
/// injected. `env -i` starts with an empty environment; only PATH, HOME,
/// caller-specified vars, and shell-internal vars (PWD, SHLVL, _) should
/// appear. Host variables like USER, LANG, HOSTNAME must NOT leak.
#[tokio::test]
async fn host_credentials_not_inherited() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir));
    sandbox.start().await.unwrap();

    let env = sandbox.env();
    let cmd = "env | sort";
    let opts = ShellOptions {
        cwd: None,
        env: vec![("SANDBOX_INJECTED", "injected_value")],
        timeout: Some(Duration::from_secs(10)),
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
    };
    let result = env.execute_shell(cmd, opts).await.unwrap();

    // The injected variable must be present.
    assert!(
        result.stdout.contains("SANDBOX_INJECTED=injected_value"),
        "explicitly injected var must be present; got:\n{}",
        result.stdout
    );

    // Host environment variables must NOT leak. These are commonly set on
    // Linux hosts but are not injected by the sandbox.
    for host_var in &["USER", "LANG", "HOSTNAME", "LOGNAME", "TERM"] {
        if std::env::var(host_var).is_ok() {
            assert!(
                !result.stdout.contains(&format!("{host_var}=")),
                "host env var {host_var} must not leak into sandbox; got:\n{}",
                result.stdout
            );
        }
    }
}

// ── Timeout kills process tree ─────────────────────────────────────────────

/// Acceptance: Cancellation/timeout terminates the entire process tree.
#[tokio::test]
async fn timeout_terminates_process_tree() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir));
    sandbox.start().await.unwrap();

    let env = sandbox.env();
    let cmd = "sleep 30; echo should_not_reach_here";
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
        "timeout must kill the process quickly; took {:?}",
        elapsed
    );
}

// ── Abort cancels running command ──────────────────────────────────────────

/// Acceptance: Abort terminates the running command.
#[tokio::test]
async fn abort_cancels_running_command() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir));
    sandbox.start().await.unwrap();

    let env = sandbox.env();
    let abort = CancellationToken::new();
    let abort_clone = abort.clone();
    let cmd = "sleep 30";

    let handle = tokio::spawn(async move {
        let opts = ShellOptions {
            cwd: None,
            env: vec![],
            timeout: None,
            abort: abort_clone,
            on_stdout: None,
            on_stderr: None,
        };
        env.execute_shell(cmd, opts).await
    });

    tokio::time::sleep(Duration::from_millis(500)).await;
    abort.cancel();

    let result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("abort must cause the command to return quickly")
        .expect("task must not panic");
    assert!(result.is_err(), "abort must produce an error");
}

// ── Workspace reset ────────────────────────────────────────────────────────

/// Acceptance: Workspace reset cannot affect paths outside the disposable root.
#[tokio::test]
async fn reset_only_affects_work_dir() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap not available");
        return;
    }
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

    let sandbox = BwrapSandbox::new(default_config(work_dir.clone()));
    sandbox.start().await.unwrap();
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

// ── Injected env vars are visible ──────────────────────────────────────────

#[tokio::test]
async fn injected_env_vars_are_visible_in_sandbox() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let sandbox = BwrapSandbox::new(default_config(work_dir));
    sandbox.start().await.unwrap();

    let env = sandbox.env();
    let cmd = "echo $MY_CUSTOM_VAR";
    let opts = ShellOptions {
        cwd: None,
        env: vec![("MY_CUSTOM_VAR", "custom_value_42")],
        timeout: Some(Duration::from_secs(10)),
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
    };
    let result = env.execute_shell(cmd, opts).await.unwrap();
    assert_eq!(result.stdout.trim(), "custom_value_42");
}

// ── fs_allowlist grants access ─────────────────────────────────────────────

#[tokio::test]
async fn fs_allowlist_grants_read_access() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();

    let allowlisted_dir = dir.path().join("shared");
    tokio::fs::create_dir_all(&allowlisted_dir).await.unwrap();
    tokio::fs::write(allowlisted_dir.join("shared.txt"), "shared_content")
        .await
        .unwrap();

    let work_dir = dir.path().join("workspace");
    tokio::fs::create_dir_all(&work_dir).await.unwrap();

    let config = SandboxConfig {
        fs_allowlist: vec![allowlisted_dir.clone()],
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
    let sandbox = BwrapSandbox::new(config);
    sandbox.start().await.unwrap();

    let env = sandbox.env();
    let cmd = format!("cat {}/shared.txt", allowlisted_dir.to_string_lossy());
    let result = env
        .execute_shell(&cmd, shell_opts())
        .await
        .unwrap();
    assert_eq!(result.stdout.trim(), "shared_content");
}
