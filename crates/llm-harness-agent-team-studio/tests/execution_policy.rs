//! Production TeamManager -> Coder harness -> fs bash tool -> ExecutionEnv.
use llm_harness_agent::session::types::SessionEntryPayload;
use llm_harness_agent_team::AgentId;
use llm_harness_agent_team_studio::{
    execution_policy,
    settings::Settings,
    team_manager::TeamManager,
    team_spec::{MemberSpec, ModelRef, TeamSpec},
    workspace::WorkspaceGrants,
};
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
use llm_harness_types::{AgentMessage, DataBlock};
use std::{path::Path, sync::Arc};

static FACTORY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn spec(id: &str, repo: &Path) -> TeamSpec {
    TeamSpec {
        id: id.into(),
        name: id.into(),
        repo_path: Some(repo.to_string_lossy().into()),
        template: None,
        template_baseline: None,
        sandbox_mode: None,
        members: vec![MemberSpec {
            id: "coder".into(),
            persona: "Execute the assigned smoke check.".into(),
            role_label: "coder".into(),
            model: ModelRef::Explicit("m".into()),
            toolkits: vec!["fs".into()],
            behaviors: vec![],
            provider_override: None,
            persona_file: None,
        }],
    }
}

fn install_command(command: &str) {
    let client: Arc<dyn llm_harness_loop::LlmClient> = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use(
            "smoke",
            "bash",
            &serde_json::json!({"command": command}).to_string(),
        ),
        MockResponse::text("Smoke check complete."),
    ]));
    llm_harness_agent_team_studio::env_cfg::install_provider_factory(Arc::new(move || {
        vec![("m".into(), client.clone())]
    }));
}

async fn output(pm: &TeamManager, id: &str) -> String {
    let harness = pm
        .get(id)
        .unwrap()
        .team
        .harness(&AgentId::new("coder"))
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        harness.prompt("Run the smoke check only."),
    )
    .await
    .unwrap()
    .unwrap();
    let entries = harness.read_active_path().await.unwrap();
    let result = entries
        .iter()
        .rev()
        .find_map(|entry| match &entry.payload {
            SessionEntryPayload::Message(AgentMessage::ToolResult(result))
                if result.tool_use_id == "smoke" =>
            {
                Some(result)
            }
            _ => None,
        })
        .expect("actual bash tool result must exist");
    assert!(!result.is_error, "bash failed: {result:?}");
    result
        .content
        .iter()
        .filter_map(|b| match b {
            DataBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join(".git")).unwrap();
    repo
}

fn authorize(data: &Path, id: &str, repo: &Path) {
    WorkspaceGrants::load(data)
        .unwrap()
        .add(repo, None)
        .unwrap();
    execution_policy::authorize(data, id).unwrap();
}

#[tokio::test]
async fn default_ignores_forged_sandbox_mode_and_blocks_policy_and_hidden_tests() {
    let _lock = FACTORY_LOCK.lock().await;
    let repo = repo();
    let data = tempfile::tempdir().unwrap();
    let hidden = tempfile::tempdir().unwrap();
    let secret = hidden.path().join("acceptance.py");
    std::fs::write(&secret, "PRIVATE-EVALUATOR-CONTENT").unwrap();
    install_command(&format!(
        "test ! -e '{}' && test ! -e '{}/trusted-host-grants.json' && test ! -e /mnt/c && echo ISOLATED",
        secret.display(),
        data.path().display()
    ));
    let mut pm = TeamManager::new(data.path());
    let mut forged = spec("ordinary", repo.path());
    forged.sandbox_mode = Some("trusted-host".into());
    let entry = pm
        .create(forged, Some(repo.path().into()), &Settings::default(), "")
        .await
        .unwrap();
    assert_eq!(entry.spec.sandbox_mode.as_deref(), Some("bwrap"));
    assert!(output(&pm, "ordinary").await.contains("ISOLATED"));
}

#[tokio::test]
async fn grant_allows_host_access_and_survives_restart_and_runtime_restoration() {
    let _lock = FACTORY_LOCK.lock().await;
    let repo = repo();
    let data = tempfile::tempdir().unwrap();
    let host = tempfile::tempdir().unwrap();
    let facility = host.path().join("approved-tool.txt");
    std::fs::write(&facility, "AUTHORIZED-HOST-FACILITY").unwrap();
    let command = format!("cat '{}'", facility.display());
    install_command(&command);
    let mut pm = TeamManager::new(data.path());
    pm.create(
        spec("development", repo.path()),
        Some(repo.path().into()),
        &Settings::default(),
        "",
    )
    .await
    .unwrap();
    authorize(data.path(), "development", repo.path());
    // Apply through the same production restart used by the operator route.
    pm.restart("development", &Settings::default(), "")
        .await
        .unwrap();
    assert_eq!(
        pm.get("development").unwrap().spec.sandbox_mode.as_deref(),
        Some("trusted-host")
    );
    assert!(
        output(&pm, "development")
            .await
            .contains("AUTHORIZED-HOST-FACILITY")
    );
    pm.shutdown_all().await;
    drop(pm);
    // Full Studio startup reads the grant and rebuilds the persisted Team.
    let client: Arc<dyn llm_harness_loop::LlmClient> = Arc::new(MockLlmClient::new(vec![]));
    let runtime = llm_harness_agent_team_studio::runtime::StudioRuntime::start(
        llm_harness_agent_team_studio::runtime::StudioConfig::from_env()
            .with_data_root(data.path())
            .with_port(0)
            .with_provider_factory(Some(Arc::new(move || vec![("m".into(), client.clone())]))),
    )
    .await
    .unwrap();
    assert_eq!(runtime.recovery().restored_teams, 1);
    let saved: TeamSpec = serde_json::from_slice(
        &std::fs::read(data.path().join("teams/development/team.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved.sandbox_mode.as_deref(), Some("trusted-host"));
    runtime.shutdown().await;
    // Revocation takes effect on reconstruction and cannot be undone by the
    // stale trusted-host field in the saved spec.
    execution_policy::revoke(data.path(), "development").unwrap();
    install_command("test ! -e /mnt/c && echo REVOKED");
    let mut restored = TeamManager::new(data.path());
    restored
        .create(saved, Some(repo.path().into()), &Settings::default(), "")
        .await
        .unwrap();
    assert_eq!(
        restored
            .get("development")
            .unwrap()
            .spec
            .sandbox_mode
            .as_deref(),
        Some("bwrap")
    );
    assert!(output(&restored, "development").await.contains("REVOKED"));
}

#[tokio::test]
async fn evaluation_overrides_host_grant_and_restores_confined() {
    let _lock = FACTORY_LOCK.lock().await;
    let repo = repo();
    let data = tempfile::tempdir().unwrap();
    let hidden = tempfile::tempdir().unwrap();
    let secret = hidden.path().join("hidden-test.py");
    std::fs::write(&secret, "PRIVATE").unwrap();
    install_command("true");
    let mut dev = TeamManager::new(data.path());
    dev.create(
        spec("evaluated", repo.path()),
        Some(repo.path().into()),
        &Settings::default(),
        "",
    )
    .await
    .unwrap();
    authorize(data.path(), "evaluated", repo.path());
    dev.shutdown_all().await;
    drop(dev);
    let command = format!(
        "test ! -e '{}' && test ! -e '{}/trusted-host-grants.json' && test ! -e /mnt/c && test ! -e /init && echo EVALUATION-ISOLATED",
        secret.display(),
        data.path().display()
    );
    install_command(&command);
    let mut pm = TeamManager::new(data.path()).evaluation_only();
    let mut forged = spec("evaluated", repo.path());
    forged.sandbox_mode = Some("trusted-host".into());
    pm.create(
        forged.clone(),
        Some(repo.path().into()),
        &Settings::default(),
        "",
    )
    .await
    .unwrap();
    assert_eq!(pm.execution_mode(), "evaluation");
    assert_eq!(
        pm.get("evaluated").unwrap().spec.sandbox_mode.as_deref(),
        Some("bwrap")
    );
    assert!(
        output(&pm, "evaluated")
            .await
            .contains("EVALUATION-ISOLATED")
    );
    install_command(&command);
    pm.restart("evaluated", &Settings::default(), "")
        .await
        .unwrap();
    assert_eq!(
        pm.get("evaluated").unwrap().spec.sandbox_mode.as_deref(),
        Some("bwrap")
    );
    assert!(
        output(&pm, "evaluated")
            .await
            .contains("EVALUATION-ISOLATED")
    );
}

#[tokio::test]
async fn grant_is_scoped_to_team_and_canonical_workspace() {
    let _lock = FACTORY_LOCK.lock().await;
    let repo = repo();
    let other = self::repo();
    let data = tempfile::tempdir().unwrap();
    install_command("true");
    let mut pm = TeamManager::new(data.path());
    pm.create(
        spec("scoped", repo.path()),
        Some(repo.path().into()),
        &Settings::default(),
        "",
    )
    .await
    .unwrap();
    authorize(data.path(), "scoped", repo.path());
    assert!(!execution_policy::is_authorized(data.path(), "another-team", repo.path()).unwrap());
    assert!(!execution_policy::is_authorized(data.path(), "scoped", other.path()).unwrap());
    pm.shutdown_all().await;
    pm.create(
        spec("scoped", other.path()),
        Some(other.path().into()),
        &Settings::default(),
        "",
    )
    .await
    .unwrap();
    assert_eq!(
        pm.get("scoped").unwrap().spec.sandbox_mode.as_deref(),
        Some("bwrap")
    );
}

#[tokio::test]
async fn workspace_containing_policy_store_is_rejected() {
    let _lock = FACTORY_LOCK.lock().await;
    let repo = repo();
    let data = repo.path().join("runtime-state");
    std::fs::create_dir(&data).unwrap();
    install_command("true");
    let mut pm = TeamManager::new(&data);
    assert!(
        pm.create(
            spec("unsafe-workspace", repo.path()),
            Some(repo.path().into()),
            &Settings::default(),
            ""
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn operator_cli_requires_acknowledgement_and_evaluation_cannot_authorize() {
    let _lock = FACTORY_LOCK.lock().await;
    let repo = repo();
    let data = tempfile::tempdir().unwrap();
    install_command("true");
    let mut pm = TeamManager::new(data.path());
    pm.create(
        spec("cli-team", repo.path()),
        Some(repo.path().into()),
        &Settings::default(),
        "",
    )
    .await
    .unwrap();
    WorkspaceGrants::load(data.path())
        .unwrap()
        .add(repo.path(), None)
        .unwrap();
    let cli = env!("CARGO_BIN_EXE_team-execution");
    let missing_ack = std::process::Command::new(cli)
        .args(["authorize", data.path().to_str().unwrap(), "cli-team"])
        .output()
        .unwrap();
    assert!(!missing_ack.status.success());
    assert!(execution_policy::grants(data.path()).unwrap().is_empty());
    let evaluation = std::process::Command::new(cli)
        .env("STUDIO_EVALUATION", "")
        .args([
            "authorize",
            data.path().to_str().unwrap(),
            "cli-team",
            "--acknowledge-unrestricted-host-access",
        ])
        .output()
        .unwrap();
    assert!(!evaluation.status.success());
    assert!(String::from_utf8_lossy(&evaluation.stderr).contains("disabled in evaluation"));
    assert!(execution_policy::grants(data.path()).unwrap().is_empty());
    let explicit = std::process::Command::new(cli)
        .args([
            "authorize",
            data.path().to_str().unwrap(),
            "cli-team",
            "--acknowledge-unrestricted-host-access",
        ])
        .output()
        .unwrap();
    assert!(explicit.status.success(), "{explicit:?}");
    assert!(execution_policy::is_authorized(data.path(), "cli-team", repo.path()).unwrap());
    pm.delete("cli-team").await.unwrap();
    assert!(execution_policy::grants(data.path()).unwrap().is_empty());
}

#[test]
fn insecure_or_symlinked_grant_store_is_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let data = tempfile::tempdir().unwrap();
    let file = data.path().join("trusted-host-grants.json");
    std::fs::write(&file, "[]").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(execution_policy::grants(data.path()).is_err());
    std::fs::remove_file(&file).unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::os::unix::fs::symlink(outside.path(), &file).unwrap();
    assert!(execution_policy::grants(data.path()).is_err());
}

#[tokio::test]
async fn evaluation_daemon_restoration_overrides_an_operator_host_grant() {
    let _lock = FACTORY_LOCK.lock().await;
    let repo = repo();
    let data = tempfile::tempdir().unwrap();
    install_command("true");
    let mut pm = TeamManager::new(data.path());
    pm.create(
        spec("restored-evaluation", repo.path()),
        Some(repo.path().into()),
        &Settings::default(),
        "",
    )
    .await
    .unwrap();
    authorize(data.path(), "restored-evaluation", repo.path());
    pm.restart("restored-evaluation", &Settings::default(), "")
        .await
        .unwrap();
    assert_eq!(
        pm.get("restored-evaluation")
            .unwrap()
            .spec
            .sandbox_mode
            .as_deref(),
        Some("trusted-host")
    );
    pm.shutdown_all().await;
    drop(pm);
    // The actual daemon/provider wiring must restore without making an LLM
    // request; no external model service is needed for this startup check.
    let mut settings = Settings::default();
    settings.models.main = "m".into();
    settings.models.cheap = "m".into();
    settings.models.strong = "m".into();
    settings.save(data.path()).unwrap();
    std::fs::write(data.path().join("apikey"), "unused-test-key").unwrap();
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = ChildGuard(
        std::process::Command::new(env!("CARGO_BIN_EXE_agent-studio"))
            .env("STUDIO_DATA_ROOT", data.path())
            .env("STUDIO_PORT", "0")
            .env("STUDIO_EVALUATION", "") // even empty means evaluation, fail closed
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let descriptor = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "evaluation daemon readiness timeout"
        );
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "evaluation daemon exited"
        );
        if let Ok(bytes) = std::fs::read(data.path().join("panel.json")) {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                break value;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    let url = descriptor["url"].as_str().unwrap();
    let (base, token) = url.split_once("/#token=").unwrap();
    use std::io::{Read, Write};
    let mut socket = std::net::TcpStream::connect(base.strip_prefix("http://").unwrap()).unwrap();
    socket
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    write!(socket, "GET /api/team/startup HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n").unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let startup: serde_json::Value =
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(startup["execution_mode"], "evaluation");
    assert_eq!(startup["recovery"]["restored_teams"], 1);
    let restored: TeamSpec = serde_json::from_slice(
        &std::fs::read(data.path().join("teams/restored-evaluation/team.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(restored.sandbox_mode.as_deref(), Some("bwrap"));
    assert!(
        execution_policy::is_authorized(data.path(), "restored-evaluation", repo.path()).unwrap()
    );
}

#[tokio::test]
#[ignore = "Requires installed Windows Blender and WSL interop; run explicitly"]
async fn blendergundam_production_coder_launches_windows_blender() {
    let _lock = FACTORY_LOCK.lock().await;
    let repo = Path::new("/home/liyufeng/Projects/blender-v0-demo");
    let protected = [
        "rx78_2.blend",
        "scripts/build_gundam.py",
        "scripts/build_gundam_phase5.py",
    ];
    let before: Vec<_> = protected
        .iter()
        .map(|p| std::fs::read(repo.join(p)).unwrap())
        .collect();
    let data = tempfile::tempdir().unwrap();
    // This is a backend smoke check through the production TeamManager and
    // Coder fs toolkit, without starting the model-editing pipeline (which
    // would capture snapshots in the real Gundam workspace).
    install_command(
        "test -x '/mnt/c/Program Files/Blender Foundation/Blender 5.2/blender.exe' && /usr/bin/wslpath -w . && '/mnt/c/Program Files/Blender Foundation/Blender 5.2/blender.exe' --version",
    );
    let mut pm = TeamManager::new(data.path());
    pm.create(
        spec("BlenderGundam", repo),
        Some(repo.into()),
        &Settings::default(),
        "",
    )
    .await
    .unwrap();
    authorize(data.path(), "BlenderGundam", repo);
    pm.restart("BlenderGundam", &Settings::default(), "")
        .await
        .unwrap();
    assert_eq!(
        pm.get("BlenderGundam")
            .unwrap()
            .spec
            .sandbox_mode
            .as_deref(),
        Some("trusted-host")
    );
    let out = output(&pm, "BlenderGundam").await;
    println!("Blender production Coder bash output:\n{out}");
    assert!(
        out.contains("Blender 5.2.2"),
        "installed Blender must actually launch: {out}"
    );
    for (p, bytes) in protected.iter().zip(before) {
        assert_eq!(
            std::fs::read(repo.join(p)).unwrap(),
            bytes,
            "protected artifact changed: {p}"
        );
    }
}
