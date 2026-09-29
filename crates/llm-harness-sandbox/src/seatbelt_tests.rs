//! Integration tests for SeatbeltSandbox isolation.
//!
//! Tests run on macOS only (the module is gated by
//! `#[cfg(all(feature = "seatbelt", target_os = "macos"))]`).
//! `sandbox-exec` is built into macOS, so no `#[ignore]` is needed —
//! `cargo test -p llm-harness-sandbox --features seatbelt` is a
//! full end-to-end verification.

use std::path::PathBuf;

use llm_harness_platform::sandbox::{ResourceLimits, Sandbox, SandboxConfig};
use llm_harness_types::{EnvError, ExecutionEnv, ShellOptions};
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
        host_binds: Vec::new(),
    }
}

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
        host_binds: Vec::new(),
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

// ── Policy compilation tests ───────────────────────────────────────────────

#[test]
fn policy_denies_network() {
    let config = default_config(PathBuf::from("/tmp/test"));
    let policy = generate_policy(&config, &PathBuf::from("/tmp/test"), &[]);
    assert!(policy.contains("(deny network*)"));
}

#[test]
fn policy_denies_sensitive_reads() {
    let config = default_config(PathBuf::from("/tmp/test"));
    let policy = generate_policy(&config, &PathBuf::from("/tmp/test"), &[]);
    // Fine-grained denylist: /Library is intentionally NOT blanket-denied
    // (toolchain shims resolve through it — see generate_policy comment).
    // Credential-bearing subtrees are denied instead.
    assert!(policy.contains(r#"(deny file-read* (subpath "/etc"))"#));
    assert!(policy.contains(r#"(deny file-read* (subpath "/private/etc"))"#));
    assert!(policy.contains(r#"(deny file-read* (subpath "/private/var/db"))"#));
    assert!(policy.contains(r#"(deny file-read* (subpath "/root"))"#));
    assert!(policy.contains(r#"(deny file-read* (subpath "/Users"))"#));
    assert!(policy.contains(r#"(deny file-read* (subpath "/Library/Keychains"))"#));
    assert!(policy.contains(r#"(deny file-read* (subpath "/Library/Preferences"))"#));
    assert!(policy.contains(r#"(deny file-read* (subpath "/System"))"#));
    assert!(!policy.contains(r#"(deny file-read* (subpath "/Library"))"#));
    assert!(
        policy.contains(r#"(allow file-read* (subpath "/Library/Developer/CommandLineTools"))"#)
    );
}

#[test]
fn policy_allows_work_dir_read_write() {
    let config = default_config(PathBuf::from("/private/tmp/seatbelt-abc"));
    let policy = generate_policy(&config, &PathBuf::from("/private/tmp/seatbelt-abc"), &[]);
    assert!(policy.contains("(allow file-read* (subpath \"/private/tmp/seatbelt-abc\"))"));
    assert!(policy.contains("(allow file-write* (subpath \"/private/tmp/seatbelt-abc\"))"));
}

#[test]
fn policy_allows_fs_allowlist_entries() {
    let config = SandboxConfig {
        fs_allowlist: vec![PathBuf::from("/data/docs")],
        ..default_config(PathBuf::from("/private/tmp/seatbelt-abc"))
    };
    let policy = generate_policy(
        &config,
        &PathBuf::from("/private/tmp/seatbelt-abc"),
        &[PathBuf::from("/data/docs")],
    );
    assert!(policy.contains("(allow file-read* (subpath \"/data/docs\"))"));
    assert!(policy.contains("(allow file-write* (subpath \"/data/docs\"))"));
}

#[test]
fn policy_denies_writes_to_private_tmp() {
    let config = default_config(PathBuf::from("/private/tmp/seatbelt-abc"));
    let policy = generate_policy(&config, &PathBuf::from("/private/tmp/seatbelt-abc"), &[]);
    // /private/tmp writes are denied, but work_dir (a subpath) is allowed
    // via last-match-wins (the allow appears after the deny).
    assert!(policy.contains(r#"(deny file-write* (subpath "/private/tmp"))"#));
    assert!(policy.contains("(allow file-write* (subpath \"/private/tmp/seatbelt-abc\"))"));
}

#[test]
fn restricted_policy_is_unchanged() {
    let config = default_config(PathBuf::from("/tmp/test"));
    let policy = generate_policy(&config, &PathBuf::from("/tmp/test"), &[]);
    assert!(policy.contains("(deny network*)"));
    assert!(!policy.contains("(allow network*)"));
    // No credential floors emitted for restricted (host paths unreachable
    // via the blanket /Users deny already).
    assert!(!policy.contains("/.ssh"));
}

#[test]
fn trusted_lan_policy_allows_network_and_enforces_credential_floors() {
    let config = SandboxConfig {
        trust_profile: llm_harness_platform::trust::TrustProfile::TrustedLan,
        ..default_config(PathBuf::from("/private/tmp/seatbelt-abc"))
    };
    let policy = generate_policy(&config, &PathBuf::from("/private/tmp/seatbelt-abc"), &[]);
    // Network opened.
    assert!(!policy.contains("(deny network*)"));
    assert!(policy.contains("(allow network*)"));
    // Credential floors: home allow first, then .ssh/Keychains deny after.
    let home = std::env::var("HOME").unwrap();
    let home_allow = policy
        .find(&format!(r#"(allow file-read* (subpath "{home}"))"#))
        .expect("trusted-lan must allow home read");
    let ssh_deny = policy
        .find(&format!(r#"(deny file-read* (subpath "{home}/.ssh"))"#))
        .expect(".ssh deny floor missing");
    assert!(
        ssh_deny > home_allow,
        "credential deny must follow home allow"
    );
    // .env floor after work_dir allow.
    let wd_allow = policy
        .find(r#"(allow file-read* (subpath "/private/tmp/seatbelt-abc"))"#)
        .unwrap();
    let env_deny = policy
        .find(r#"(deny file-read* (literal "/private/tmp/seatbelt-abc/.env"))"#)
        .expect(".env deny floor missing");
    assert!(env_deny > wd_allow);
    // System write floors still present.
    assert!(policy.contains(r#"(deny file-write* (subpath "/"))"#));
}

#[test]
fn trusted_full_policy_near_host_with_floors() {
    let config = SandboxConfig {
        trust_profile: llm_harness_platform::trust::TrustProfile::TrustedFull,
        ..default_config(PathBuf::from("/private/tmp/seatbelt-abc"))
    };
    let policy = generate_policy(&config, &PathBuf::from("/private/tmp/seatbelt-abc"), &[]);
    assert!(!policy.contains("(deny network*)"));
    let home = std::env::var("HOME").unwrap();
    assert!(policy.contains(&format!(r#"(allow file-read* (subpath "{home}"))"#)));
    assert!(policy.contains(&format!(r#"(allow file-write* (subpath "{home}"))"#)));
    // Floors hold: .ssh / Keychains denied after the home allow.
    assert!(policy.contains(&format!(r#"(deny file-read* (subpath "{home}/.ssh"))"#)));
    assert!(policy.contains(&format!(
        r#"(deny file-read* (subpath "{home}/Library/Keychains"))"#
    )));
    // /etc write floor still present.
    assert!(policy.contains(r#"(deny file-write* (subpath "/"))"#));
}

// ── File API tests ─────────────────────────────────────────────────────────

fn make_env(tmp: &tempfile::TempDir) -> SeatbeltEnv {
    let work_dir_canon = std::fs::canonicalize(tmp.path()).unwrap();
    let config = default_config(work_dir_canon.clone());
    SeatbeltEnv::new(
        work_dir_canon.clone(),
        work_dir_canon,
        None,
        vec![],
        config,
        false,
        std::env::temp_dir().join("test-policy.sb"),
    )
}

fn make_env_with_policy(tmp: &tempfile::TempDir) -> SeatbeltEnv {
    let work_dir_canon = std::fs::canonicalize(tmp.path()).unwrap();
    let config = default_config(work_dir_canon.clone());
    let policy_id = uuid::Uuid::new_v4();
    let policy_path = std::env::temp_dir().join(format!("seatbelt-test-{policy_id}.sb"));
    let policy = generate_policy(&config, &work_dir_canon, &[]);
    std::fs::write(&policy_path, policy).unwrap();
    SeatbeltEnv::new(
        work_dir_canon.clone(),
        work_dir_canon,
        None,
        vec![],
        config,
        false,
        policy_path,
    )
}

#[tokio::test]
async fn file_read_write_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let env = make_env(&tmp);
    let path = std::path::Path::new("test.txt");
    let abort = CancellationToken::new();

    env.write_file(path, b"hello", abort.clone()).await.unwrap();
    let content = env.read_text_file(path, abort).await.unwrap();
    assert_eq!(content, "hello");
}

#[tokio::test]
async fn file_rejects_path_outside_work_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let env = make_env(&tmp);
    let abort = CancellationToken::new();

    let result = env
        .read_text_file(std::path::Path::new("/etc/passwd"), abort)
        .await;
    assert!(matches!(result, Err(EnvError::PermissionDenied(_))));
}

#[tokio::test]
async fn file_rejects_parent_dir_traversal() {
    let tmp = tempfile::tempdir().unwrap();
    let env = make_env(&tmp);
    let abort = CancellationToken::new();

    let result = env
        .read_text_file(std::path::Path::new("../../etc/passwd"), abort)
        .await;
    assert!(matches!(result, Err(EnvError::PermissionDenied(_))));
}

#[tokio::test]
async fn file_append_works() {
    let tmp = tempfile::tempdir().unwrap();
    let env = make_env(&tmp);
    let path = std::path::Path::new("log.txt");
    let abort = CancellationToken::new();

    env.append_file(path, b"line1\n", abort.clone())
        .await
        .unwrap();
    env.append_file(path, b"line2\n", abort.clone())
        .await
        .unwrap();
    let content = env.read_text_file(path, abort).await.unwrap();
    assert_eq!(content, "line1\nline2\n");
}

#[tokio::test]
async fn file_create_and_list_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let env = make_env(&tmp);
    let abort = CancellationToken::new();

    env.create_dir(std::path::Path::new("subdir"), true, abort.clone())
        .await
        .unwrap();
    let entries = env
        .list_dir(std::path::Path::new("."), abort)
        .await
        .unwrap();
    assert!(entries.iter().any(|e| e.path.ends_with("subdir")));
}

// ── Shell isolation tests ──────────────────────────────────────────────────

#[tokio::test]
async fn shell_echo_hello() {
    let tmp = tempfile::tempdir().unwrap();
    let env = make_env_with_policy(&tmp);
    let opts = shell_opts();
    let output = env.execute_shell("echo hello", opts).await.unwrap();
    assert!(output.exit_code == 0);
    assert_eq!(output.stdout.trim(), "hello");
}

#[tokio::test]
async fn shell_cannot_read_etc_passwd() {
    let tmp = tempfile::tempdir().unwrap();
    let env = make_env_with_policy(&tmp);
    let opts = shell_opts();
    let result = env.execute_shell("cat /etc/passwd", opts).await;
    // sandbox-exec policy denies file-read* /etc/*, so cat should fail.
    assert!(result.is_err() || result.unwrap().exit_code != 0);
}

#[tokio::test]
async fn shell_cannot_write_outside_work_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let env = make_env_with_policy(&tmp);
    let opts = shell_opts();
    let result = env
        .execute_shell("touch /tmp/seatbelt-escape-test-$$", opts)
        .await;
    assert!(result.is_err() || result.unwrap().exit_code != 0);
}

#[tokio::test]
async fn shell_can_write_in_work_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let env = make_env_with_policy(&tmp);
    let work_dir = env.working_dir().to_path_buf();
    let opts = shell_opts();
    let result = env.execute_shell("touch testfile", opts).await;
    assert!(result.is_ok());
    assert!(work_dir.join("testfile").exists());
}

#[tokio::test]
async fn shell_env_vars_injected() {
    let tmp = tempfile::tempdir().unwrap();
    let env = make_env_with_policy(&tmp);
    let opts = ShellOptions {
        cwd: None,
        env: vec![("TEST_VAR", "injected")],
        timeout: None,
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
        max_output_bytes: None,
    };
    let output = env.execute_shell("echo $TEST_VAR", opts).await.unwrap();
    assert_eq!(output.stdout.trim(), "injected");
}

// ── Lifecycle tests ────────────────────────────────────────────────────────

#[tokio::test]
async fn sandbox_start_reset_shutdown_cycle() {
    let config = owned_config();
    let sandbox = SeatbeltSandbox::new(config).unwrap();
    let work_dir = sandbox.env.work_dir.clone();

    // start
    sandbox.start().await.unwrap();
    assert!(sandbox.is_running());

    // write a file
    let env = sandbox.env();
    env.write_file(
        std::path::Path::new("test.txt"),
        b"data",
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(work_dir.join("test.txt").exists());

    // reset clears the file
    sandbox.reset().await.unwrap();
    assert!(!work_dir.join("test.txt").exists());

    // shutdown
    Box::new(sandbox).shutdown().await.unwrap();
}

#[tokio::test]
async fn sandbox_start_writes_and_deletes_policy_file() {
    let tmp = tempfile::tempdir().unwrap();
    let work_dir = std::fs::canonicalize(tmp.path()).unwrap();
    let config = default_config(work_dir);
    let sandbox = SeatbeltSandbox::new(config).unwrap();
    let policy_path = sandbox.env.policy_path.clone();

    assert!(!policy_path.exists());
    sandbox.start().await.unwrap();
    assert!(policy_path.exists());

    Box::new(sandbox).shutdown().await.unwrap();
    assert!(!policy_path.exists());
}

#[tokio::test]
async fn sandbox_owned_work_dir_deleted_on_shutdown() {
    let config = owned_config();
    let sandbox = SeatbeltSandbox::new(config).unwrap();
    let work_dir = sandbox.env.work_dir.clone();

    sandbox.start().await.unwrap();
    assert!(work_dir.exists());

    Box::new(sandbox).shutdown().await.unwrap();
    assert!(!work_dir.exists());
}

#[tokio::test]
async fn sandbox_validate_config_rejects_denylist() {
    let config = SandboxConfig {
        fs_denylist: vec![PathBuf::from("/secret")],
        ..default_config(PathBuf::from("/tmp/test"))
    };
    assert!(SeatbeltSandbox::new(config).is_err());
}

#[tokio::test]
async fn sandbox_validate_config_rejects_net_allowlist() {
    let config = SandboxConfig {
        net_allowlist: vec![llm_harness_platform::sandbox::NetRule {
            host: "example.com".to_string(),
            port_range: None,
        }],
        ..default_config(PathBuf::from("/tmp/test"))
    };
    assert!(SeatbeltSandbox::new(config).is_err());
}

#[tokio::test]
async fn sandbox_validate_config_rejects_resource_limits() {
    let config = SandboxConfig {
        resource_limits: ResourceLimits {
            max_cpus: Some(2),
            max_processes: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        ..default_config(PathBuf::from("/tmp/test"))
    };
    assert!(SeatbeltSandbox::new(config).is_err());
}
