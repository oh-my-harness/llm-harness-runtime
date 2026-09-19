use axum::http::{StatusCode, header};
use llm_harness_runtime_app::app::{RuntimeApp, RuntimeAppConfig};
#[cfg(unix)]
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::Command;
#[cfg(unix)]
use std::process::Stdio;
use std::sync::Arc;
#[cfg(unix)]
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_root(test: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "runtime-app-{test}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[tokio::test]
async fn app_serves_embedded_react_frontend_and_keeps_fallback() {
    let root = temp_root("frontend");
    std::fs::create_dir_all(&root).unwrap();
    let app = RuntimeApp::start(
        RuntimeAppConfig::from_env()
            .with_data_root(&root)
            .with_provider_factory(Some(provider_factory())),
    )
    .await
    .unwrap();

    let addr = app.bound_addr().unwrap();
    let client = reqwest::Client::new();
    let response = client
        .get(format!("http://{addr}/app/agent-team"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/html; charset=utf-8"
    );
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .unwrap(),
        "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );
    let index = response.text().await.unwrap();
    assert!(index.contains("<div id=\"root\"></div>"));

    let script_path = index
        .split("src=\"/app/agent-team/")
        .nth(1)
        .and_then(|suffix| suffix.split('"').next())
        .unwrap();
    let response = client
        .get(format!("http://{addr}/app/agent-team/{script_path}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/javascript; charset=utf-8"
    );
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "public, max-age=31536000, immutable"
    );

    let response = client
        .get(format!(
            "http://{addr}/app/agent-team/%2e%2e/%2e%2e/Cargo.toml"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = client
        .get(format!("http://{addr}/app/team.html"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let expected_panel_url = app.panel_url().unwrap();
    assert!(expected_panel_url.contains("/app/agent-team#token="));
    let descriptor = std::fs::read_to_string(app.panel_descriptor_path()).unwrap();
    let descriptor: serde_json::Value = serde_json::from_str(&descriptor).unwrap();
    assert_eq!(descriptor["url"], expected_panel_url);

    app.shutdown().await;
    std::fs::remove_dir_all(&root).unwrap();
}

fn provider_factory() -> llm_harness_agent_team_studio::env_cfg::ProviderFactory {
    Arc::new(|| {
        vec![(
            "default".to_string(),
            Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]))
                as Arc<dyn llm_harness_loop::LlmClient>,
        )]
    })
}

#[tokio::test]
async fn app_exposes_health_and_diagnostics_with_auth() {
    let root = temp_root("health");
    std::fs::create_dir_all(&root).unwrap();
    let app = RuntimeApp::start(
        RuntimeAppConfig::from_env()
            .with_data_root(&root)
            .with_provider_factory(Some(provider_factory())),
    )
    .await
    .unwrap();

    let addr = app.bound_addr().unwrap();
    let token = app.auth_token().unwrap().to_owned();
    let client = reqwest::Client::new();

    let response = client
        .get(format!("http://{addr}/api/app/health"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.json::<serde_json::Value>().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["app"], "llm-harness-runtime-app");
    assert_eq!(body["startup_recovery"]["status"], "healthy");
    assert_eq!(body["feature_health"]["agent-team"]["status"], "ok");

    let response = client
        .get(format!("http://{addr}/api/app/diagnostics"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.json::<serde_json::Value>().await.unwrap();
    assert!(!body["lifecycle_events"].as_array().unwrap().is_empty());

    let response = client
        .get(format!("http://{addr}/api/app/data-migration"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.json::<serde_json::Value>().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["current_data_root"], serde_json::json!(root));
    assert_eq!(body["binary"], "llm-harness-app");
    assert_eq!(body["requires_app_shutdown"], true);

    let response = client
        .get(format!("http://{addr}/api/app/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    app.shutdown().await;
    std::fs::remove_dir_all(&root).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn app_binary_starts_and_shuts_down_cleanly() {
    let root = temp_root("binary");
    std::fs::create_dir_all(&root).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_llm-harness-app"))
        .env("RUNTIME_APP_DATA_ROOT", &root)
        .env("RUNTIME_APP_PORT", "0")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut descriptor_line = String::new();
    let started = Instant::now();
    loop {
        descriptor_line.clear();
        let bytes = reader.read_line(&mut descriptor_line).unwrap();
        assert!(
            bytes > 0,
            "llm-harness-app exited before reporting descriptor"
        );
        if descriptor_line.starts_with("RUNTIME_APP_PANEL_DESCRIPTOR=") {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(30));
    }
    let descriptor_path: PathBuf = descriptor_line
        .strip_prefix("RUNTIME_APP_PANEL_DESCRIPTOR=")
        .unwrap()
        .trim_end_matches(['\n', '\r'])
        .into();
    assert_eq!(descriptor_path, root.join("panel.json"));

    tokio::time::sleep(Duration::from_millis(100)).await;
    let pid = child.id();
    let sent = unsafe { libc::kill(pid.try_into().unwrap(), libc::SIGTERM) };
    assert_eq!(sent, 0, "failed to send SIGTERM to llm-harness-app");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "llm-harness-app exited with {status}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "llm-harness-app did not stop within 10s"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let lifecycle_log = root.join("logs").join("runtime.jsonl");
    let lifecycle_log = std::fs::read_to_string(lifecycle_log).unwrap();
    let events: Vec<serde_json::Value> = lifecycle_log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let shutdown_signal = events
        .iter()
        .find(|event| event["data"]["event"] == "shutdown_signal")
        .unwrap();
    assert_eq!(shutdown_signal["data"]["source"], "sigterm");
    let shutdown = events.last().unwrap();
    assert_eq!(shutdown["data"]["event"], "shutdown");
    assert_eq!(shutdown["data"]["reason"], "normal");

    assert!(!descriptor_path.exists());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn app_binary_rejects_invalid_shutdown_timeout() {
    let output = Command::new(env!("CARGO_BIN_EXE_llm-harness-app"))
        .env("RUNTIME_APP_SHUTDOWN_TIMEOUT_SECS", "0")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("invalid lifecycle configuration"));
    assert!(stderr.contains("expected between 1s and 3600s"));
}

#[test]
fn app_binary_migrates_and_rolls_back_data_root() {
    let parent = temp_root("migration");
    std::fs::create_dir_all(&parent).unwrap();
    let source = parent.join("source");
    std::fs::create_dir_all(source.join("teams/demo")).unwrap();
    std::fs::write(source.join("settings.json"), "{\"scout_interval_secs\":17}").unwrap();
    std::fs::write(
        source.join("teams/demo/team.json"),
        "{\"id\":\"demo\",\"name\":\"Demo\",\"members\":[]}",
    )
    .unwrap();
    let destination = parent.join("runtime-app-migration-destination");
    let backup = parent.join("runtime-app-migration-backup");

    let output = Command::new(env!("CARGO_BIN_EXE_llm-harness-app"))
        .args([
            "migrate-data",
            "--from",
            source.to_str().unwrap(),
            "--to",
            destination.to_str().unwrap(),
            "--backup",
            backup.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "migration failed: {output:?}");
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["operation"], "migrate");
    assert!(destination.join("settings.json").exists());
    assert!(destination.join("teams/demo/team.json").exists());
    assert!(backup.join("settings.json").exists());
    assert!(!source.exists());

    let output = Command::new(env!("CARGO_BIN_EXE_llm-harness-app"))
        .args([
            "rollback-data",
            "--from",
            source.to_str().unwrap(),
            "--to",
            destination.to_str().unwrap(),
            "--backup",
            backup.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "rollback failed: {output:?}");
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["operation"], "rollback");
    assert!(source.join("settings.json").exists());
    assert!(source.join("teams/demo/team.json").exists());
    assert!(!destination.exists());
    assert!(!backup.exists());

    std::fs::remove_dir_all(parent).unwrap();
}
