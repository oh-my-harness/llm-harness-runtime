#![cfg(feature = "live-llm")]

//! Tests: OsEnvSandbox file I/O, shell execution, and resource limits.
//!
//! These tests exercise the sandbox directly — no LLM provider required.
//! Verifies:
//! - File write/read roundtrip via `ExecutionEnv::write_file` / `read_text_file`
//! - Shell execution returns captured stdout
//! - Non-zero exit code is surfaced faithfully
//! - `ResourceLimits.timeout` interrupts a long-running command

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use llm_harness_platform::sandbox::{ResourceLimits, Sandbox, SandboxConfig};
use llm_harness_sandbox::OsEnvSandbox;
use llm_harness_types::{EnvError, ExecutionEnv, ShellOptions, ShellOutput};
use tokio_util::sync::CancellationToken;

// ── Helpers ───────────────────────────────────────────────────────────────

/// `ResourceLimits` with no caps.
fn no_limits() -> ResourceLimits {
    ResourceLimits {
        max_cpus: None,
        max_processes: None,
        max_memory_mb: None,
        max_disk_mb: None,
        timeout: None,
    }
}

/// Build + return an `OsEnvSandbox` rooted at a tempdir work directory.
fn make_sandbox(tmp: &tempfile::TempDir, limits: ResourceLimits) -> OsEnvSandbox {
    let config = SandboxConfig {
        fs_allowlist: vec![tmp.path().to_path_buf()],
        fs_denylist: vec![],
        net_allowlist: vec![],
        resource_limits: limits,
        work_dir: Some(tmp.path().to_path_buf()),
        trust_profile: Default::default(),
        host_binds: Vec::new(),
    };
    OsEnvSandbox::new(config)
}

/// Default `ShellOptions`: no extra env, no timeout, no streaming callbacks.
fn default_opts() -> ShellOptions<'static> {
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

// ── Tests ─────────────────────────────────────────────────────────────────

/// Write a file through `ExecutionEnv::write_file`, read it back with
/// `read_text_file`, and assert the content matches byte-for-byte.
#[tokio::test]
async fn file_write_read_roundtrip() {
    let tmp = tempfile::tempdir().expect("tempdir failed");
    let sandbox = make_sandbox(&tmp, no_limits());
    sandbox.start().await.expect("sandbox start failed");
    assert!(sandbox.is_running(), "sandbox should report running");

    let env: Arc<dyn ExecutionEnv> = sandbox.env();
    let abort = CancellationToken::new();
    let file_path = Path::new("roundtrip.txt");
    let content = "Hello, sandbox!\nline two\n";

    env.write_file(file_path, content.as_bytes(), abort.clone())
        .await
        .expect("write_file failed");

    let read_back = env
        .read_text_file(file_path, abort)
        .await
        .expect("read_text_file failed");

    assert_eq!(read_back, content, "file I/O roundtrip: content must match");
}

/// `execute_shell` captures stdout from a simple echo command.
#[tokio::test]
async fn shell_execution_returns_output() {
    let tmp = tempfile::tempdir().expect("tempdir failed");
    let sandbox = make_sandbox(&tmp, no_limits());
    sandbox.start().await.expect("sandbox start failed");

    let env: Arc<dyn ExecutionEnv> = sandbox.env();
    let out: ShellOutput = env
        .execute_shell("echo 'hello world'", default_opts())
        .await
        .expect("execute_shell failed");

    assert_eq!(
        out.exit_code, 0,
        "echo should exit zero (exit_code={})",
        out.exit_code
    );
    assert!(
        out.stdout.contains("hello world"),
        "stdout should contain 'hello world', got: {:?}",
        out.stdout
    );
}

/// A failing command surfaces a non-zero exit code. OsEnvSandbox returns
/// `Err(EnvError::ShellFailed { exit_code, .. })` for non-zero exits (per its
/// own test `execute_shell_nonzero_exit_returns_err`); we assert the exit
/// code is faithfully reported inside that error.
#[tokio::test]
async fn shell_nonzero_exit_code() {
    let tmp = tempfile::tempdir().expect("tempdir failed");
    let sandbox = make_sandbox(&tmp, no_limits());
    sandbox.start().await.expect("sandbox start failed");

    let env: Arc<dyn ExecutionEnv> = sandbox.env();
    let result = env.execute_shell("exit 42", default_opts()).await;

    match result {
        Ok(out) => {
            assert_ne!(
                out.exit_code, 0,
                "exit 42 must produce a non-zero exit code (got {})",
                out.exit_code
            );
        }
        Err(EnvError::ShellFailed { exit_code, .. }) => {
            assert_eq!(
                exit_code, 42,
                "exit code should be faithfully reported as 42 (got {})",
                exit_code
            );
        }
        Err(e) => panic!("expected ShellFailed for non-zero exit, got: {e:?}"),
    }
}

/// `ResourceLimits.timeout` (mirrored into `ShellOptions.timeout`) interrupts
/// a long-running command well before it would complete on its own.
#[tokio::test]
async fn timeout_kills_long_command() {
    let tmp = tempfile::tempdir().expect("tempdir failed");
    let limits = ResourceLimits {
        max_cpus: None,
        max_processes: None,
        max_memory_mb: None,
        max_disk_mb: None,
        timeout: Some(Duration::from_millis(100)),
    };
    let sandbox = make_sandbox(&tmp, limits);
    sandbox.start().await.expect("sandbox start failed");

    let env: Arc<dyn ExecutionEnv> = sandbox.env();

    // The per-call `ShellOptions.timeout` is what enforces the wall-clock
    // limit at execution time; mirror the sandbox config here.
    let mut opts = default_opts();
    opts.timeout = Some(Duration::from_millis(100));

    let start = Instant::now();
    let result = env.execute_shell("sleep 5", opts).await;
    let elapsed = start.elapsed();

    // The command should be interrupted well before 5s. Whether it surfaces
    // as an error or an output with a timeout-indicating exit code, either is
    // acceptable; the defining assertion is the wall-clock bound.
    match result {
        Ok(out) => {
            assert!(
                out.exit_code != 0,
                "sleep 5 should not exit zero after timeout (exit_code={})",
                out.exit_code
            );
        }
        Err(e) => {
            // Timeout surfaced as an error — also acceptable.
            eprintln!("sleep 5 failed as expected: {e}");
        }
    }
    assert!(
        elapsed < Duration::from_secs(3),
        "timeout did not interrupt the command in time: {:?}",
        elapsed
    );
}
