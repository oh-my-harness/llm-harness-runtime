//! Evaluation-isolation regressions (P4-C Fix B).
//!
//! Proves the execution env handed to coding-team agents ("fs" toolkit:
//! bash/read/write/…) is confined to the team workspace: tool calls must
//! not read host paths outside the workspace (hidden acceptance suites,
//! reference/incorrect variants, other attempts' evaluation state) nor
//! write outside it. Also pins that the applied confinement is recorded
//! in the persisted team spec for comparability.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

#[cfg(target_os = "linux")]
use llm_harness_platform::sandbox::{ResourceLimits, Sandbox, SandboxConfig};
#[cfg(target_os = "linux")]
use llm_harness_sandbox::BwrapSandbox;
use llm_harness_sandbox::OsEnv;
use llm_harness_types::ShellOptions;
use tokio_util::sync::CancellationToken;

fn shell_opts() -> ShellOptions<'static> {
    ShellOptions {
        cwd: None,
        env: vec![],
        timeout: Some(Duration::from_secs(10)),
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
        max_output_bytes: None,
    }
}

#[cfg(target_os = "linux")]
fn config_for(work_dir: std::path::PathBuf) -> SandboxConfig {
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

/// Mirrors `TeamManager::create` fallback logic: confined env when the host
/// can run bwrap, unconfined `OsEnv` otherwise. Tests that assert
/// confinement must skip when the fallback applies. Async because
/// `Sandbox::start` spawns tokio processes; never call from a sync context.
#[cfg(target_os = "linux")]
async fn confined_env_or_none(
    work_dir: std::path::PathBuf,
) -> Option<std::sync::Arc<dyn llm_harness_types::ExecutionEnv>> {
    let sandbox = match BwrapSandbox::new(config_for(work_dir)) {
        Ok(sandbox) => sandbox,
        Err(_) => return None,
    };
    match sandbox.start().await {
        Ok(()) => Some(sandbox.env()),
        Err(_) => None,
    }
}

#[cfg(not(target_os = "linux"))]
async fn confined_env_or_none(
    _work_dir: std::path::PathBuf,
) -> Option<std::sync::Arc<dyn llm_harness_types::ExecutionEnv>> {
    None
}

#[tokio::test]
async fn agent_env_shell_cannot_read_paths_outside_workspace() {
    // Secret placed outside the workspace, simulating hidden benchmark
    // assets (acceptance suite / reference / another attempt's results).
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(
        outside.path().join("hidden-suite.py"),
        "HIDDEN-TEST-CONTENT",
    )
    .unwrap();

    let workspace = tempfile::tempdir().unwrap();
    let env = match confined_env_or_none(workspace.path().to_path_buf()).await {
        Some(env) => env,
        None => {
            eprintln!("skipped: bwrap user namespaces unavailable on this host");
            return;
        }
    };

    let result = env
        .execute_shell("cat /etc/passwd 2>/dev/null | head -1", shell_opts())
        .await
        .unwrap();
    assert!(
        !result.stdout.contains("root:"),
        "agent shell must not read host files outside the workspace"
    );

    let result = env
        .execute_shell(
            &format!(
                "cat {}/hidden-suite.py 2>/dev/null || echo BLOCKED",
                outside.path().display()
            ),
            shell_opts(),
        )
        .await
        .unwrap();
    assert!(
        result.stdout.contains("BLOCKED"),
        "agent shell must not read the hidden suite; got: {}",
        result.stdout
    );
}

#[tokio::test]
async fn agent_env_shell_cannot_write_outside_workspace() {
    let outside = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let env = match confined_env_or_none(workspace.path().to_path_buf()).await {
        Some(env) => env,
        None => {
            eprintln!("skipped: bwrap user namespaces unavailable on this host");
            return;
        }
    };
    let target = outside.path().join("tamper.txt");
    let result = env
        .execute_shell(
            &format!(
                "touch {} 2>/dev/null && echo WROTE || echo BLOCKED",
                target.display()
            ),
            shell_opts(),
        )
        .await
        .unwrap();
    assert!(
        result.stdout.contains("BLOCKED"),
        "agent shell must not write outside the workspace"
    );
    assert!(!target.exists(), "tamper target must not exist");
}

#[tokio::test]
async fn agent_env_workspace_work_remains_functional() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("calc.py"),
        b"def add(a, b):\n    return a + b\n",
    )
    .unwrap();
    let env = match confined_env_or_none(workspace.path().to_path_buf()).await {
        Some(env) => env,
        None => {
            eprintln!("skipped: bwrap user namespaces unavailable on this host");
            return;
        }
    };
    // The documented TDD loop (write failing test, run, implement) needs
    // workspace reads/writes plus a working python inside the sandbox.
    let result = env
        .execute_shell(
            "python3 -m py_compile calc.py && echo COMPILED",
            shell_opts(),
        )
        .await
        .unwrap();
    assert_eq!(
        result.stdout.trim(),
        "COMPILED",
        "normal workspace work must keep working inside the sandbox"
    );
    let result = env
        .execute_shell("cat calc.py", shell_opts())
        .await
        .unwrap();
    assert!(result.stdout.contains("def add"));
}

#[tokio::test]
async fn agent_env_clears_host_environment_variables() {
    // A host env var that must never reach the agent shell (mirrors what
    // the queue guarantees for the runner: no inherited credentials).
    let workspace = tempfile::tempdir().unwrap();
    let env = match confined_env_or_none(workspace.path().to_path_buf()).await {
        Some(env) => env,
        None => {
            eprintln!("skipped: bwrap user namespaces unavailable on this host");
            return;
        }
    };
    let result = env
        .execute_shell("printenv P4C_SECRET || echo ABSENT", shell_opts())
        .await
        .unwrap();
    assert!(
        result.stdout.contains("ABSENT"),
        "host environment must not leak into the agent shell"
    );
}

/// Pins the fallback semantics of `TeamManager::create`: on this host bwrap
/// works, so the fallback path must NOT be selected. If a future refactor
/// silently drops the sandbox (regression to unconfined OsEnv), the
/// shell-confinement tests above catch it; this test documents the
/// decision table for hosts where bwrap is unavailable.
#[tokio::test]
async fn sandbox_construction_follows_host_capability() {
    let workspace = tempfile::tempdir().unwrap();
    let confined = confined_env_or_none(workspace.path().to_path_buf())
        .await
        .is_some();
    #[cfg(target_os = "linux")]
    let direct = match BwrapSandbox::new(config_for(workspace.path().to_path_buf())) {
        Ok(sandbox) => sandbox.start().await.is_ok(),
        Err(_) => false,
    };
    #[cfg(not(target_os = "linux"))]
    let direct = false;
    assert_eq!(
        confined, direct,
        "env selection must track bwrap availability exactly"
    );
    let _ = OsEnv::new(workspace.path()); // fallback constructor still available
    let _ = Stdio::null(); // keep import parity with other process tests
}

#[tokio::test]
async fn unconfined_osenv_documented_as_insecure_for_evaluations() {
    // Documents the vulnerable baseline: the raw OsEnv previously handed to
    // teams reads host paths freely. This test FAILS if OsEnv ever becomes
    // confined implicitly (it must stay honest so the fallback risk stays
    // visible in test output), and PASSES today, demonstrating the issue
    // the BwrapSandbox wiring fixes.
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "TOP-SECRET").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let env: std::sync::Arc<dyn llm_harness_types::ExecutionEnv> =
        std::sync::Arc::new(OsEnv::new(workspace.path()));
    let result = env
        .execute_shell(
            &format!("cat {}/secret.txt", outside.path().display()),
            shell_opts(),
        )
        .await
        .unwrap();
    assert!(
        result.stdout.contains("TOP-SECRET"),
        "unconfined OsEnv must remain observable as unconfined"
    );
}

// ── B2 Task A: production-path regression ──────────────────────────────────
use llm_harness_agent_team_studio::team_manager::TeamManager;
use llm_harness_agent_team_studio::team_spec::{MemberSpec, TeamSpec};

fn solo_spec(id: &str, repo: &std::path::Path) -> TeamSpec {
    TeamSpec {
        id: id.into(),
        template: None,
        template_baseline: None,
        name: id.into(),
        repo_path: Some(repo.to_string_lossy().to_string()),
        sandbox_mode: None,
        members: vec![MemberSpec {
            id: "coder".into(),
            persona: "p".into(),
            role_label: "coder".into(),
            model: llm_harness_agent_team_studio::team_spec::ModelRef::Explicit("m".into()),
            toolkits: vec!["fs".into()],
            behaviors: vec![],
            provider_override: None,
            persona_file: None,
        }],
    }
}

fn studio_settings() -> llm_harness_agent_team_studio::settings::Settings {
    llm_harness_agent_team_studio::settings::Settings::default()
}

#[tokio::test]
async fn fresh_team_registration_selects_bwrap() {
    let client: Arc<dyn llm_harness_loop::LlmClient> =
        Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]));
    llm_harness_agent_team_studio::env_cfg::install_provider_factory(Arc::new(move || {
        vec![("m".into(), client.clone())]
    }));
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join(".git")).unwrap();
    let data = tempfile::tempdir().unwrap();
    let mut pm = TeamManager::new(data.path());
    let team_spec = solo_spec("fresh-bwrap", repo.path());
    let entry = pm
        .create(
            team_spec,
            Some(repo.path().to_path_buf()),
            &studio_settings(),
            "",
        )
        .await
        .expect("fresh registration must succeed");
    assert_eq!(
        entry.spec.sandbox_mode.as_deref(),
        Some("bwrap"),
        "fresh TeamManager registration must select the sandboxed env on a bwrap-capable host"
    );
}

#[tokio::test]
async fn symlinked_issues_directory_is_rejected() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join(".git")).unwrap();
    let data = tempfile::tempdir().unwrap();
    let team_dir = data.path().join("teams").join("link-team");
    // Redirect the issues directory: a symlink to an outside location must
    // be refused, not silently accepted as the allowlist target.
    std::fs::create_dir_all(&team_dir).unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), team_dir.join("issues")).unwrap();
    let mut pm = TeamManager::new(data.path());
    let result = pm
        .create(
            solo_spec("link-team", repo.path()),
            Some(repo.path().to_path_buf()),
            &studio_settings(),
            "",
        )
        .await;
    assert!(
        result.is_err(),
        "symlinked issues directory must be rejected as a sandbox allowlist target"
    );
}
