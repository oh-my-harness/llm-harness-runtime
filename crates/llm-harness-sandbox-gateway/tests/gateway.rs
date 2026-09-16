use std::sync::Arc;
use std::time::{Duration, Instant};

use llm_harness_platform::sandbox::{ResourceLimits, Sandbox, SandboxConfig};
use llm_harness_sandbox_gateway::{GatewayConfig, GatewayPrincipalConfig, SandboxGateway};
use llm_harness_sandbox_remote::{RemoteSandbox, RemoteSandboxClient, RemoteSandboxClientConfig};
use llm_harness_types::{EnvError, ShellOptions};
use tokio_util::sync::CancellationToken;

const TOKEN_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TOKEN_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct TestGateway {
    base_url: String,
    _gateway: SandboxGateway,
}

async fn start_gateway(principals: Vec<GatewayPrincipalConfig>) -> TestGateway {
    start_gateway_with_config(GatewayConfig::new(principals).expect("gateway config")).await
}

async fn start_gateway_with_config(config: GatewayConfig) -> TestGateway {
    let gateway = SandboxGateway::new(config).expect("gateway");
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind gateway");
    let address = listener.local_addr().expect("gateway address");
    let server_gateway = gateway.clone();
    tokio::spawn(async move {
        axum::serve(listener, server_gateway.router())
            .await
            .expect("gateway server");
    });
    TestGateway {
        base_url: format!("http://{address}"),
        _gateway: gateway,
    }
}

fn principal(id: &str, token: &str) -> GatewayPrincipalConfig {
    GatewayPrincipalConfig {
        principal: id.to_owned(),
        bearer_token: token.to_owned(),
        fs_allowlist: Vec::new(),
    }
}

fn remote_client(base_url: &str, token: &str) -> Arc<RemoteSandboxClient> {
    let config = RemoteSandboxClientConfig::new(base_url, token).expect("client config");
    Arc::new(RemoteSandboxClient::new(config).expect("client"))
}

fn sandbox_config() -> SandboxConfig {
    SandboxConfig {
        fs_allowlist: Vec::new(),
        fs_denylist: Vec::new(),
        net_allowlist: Vec::new(),
        resource_limits: ResourceLimits {
            max_cpus: None,
            max_processes: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: None,
    }
}

fn bwrap_available() -> bool {
    std::process::Command::new("bwrap")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

async fn connect(gateway: &TestGateway, token: &str) -> RemoteSandbox {
    RemoteSandbox::connect(sandbox_config(), remote_client(&gateway.base_url, token))
        .await
        .expect("connect remote sandbox")
}

fn shell_options(timeout: Duration) -> ShellOptions<'static> {
    shell_options_with_abort(timeout, CancellationToken::new())
}

fn shell_options_with_abort(timeout: Duration, abort: CancellationToken) -> ShellOptions<'static> {
    ShellOptions {
        cwd: None,
        env: Vec::new(),
        timeout: Some(timeout),
        abort,
        on_stdout: None,
        on_stderr: None,
        max_output_bytes: Some(64 * 1024),
    }
}

#[tokio::test]
async fn gateway_rejects_invalid_bearer_tokens() {
    let gateway = start_gateway(vec![principal("tenant-a", TOKEN_A)]).await;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/sandboxes/not-a-sandbox", gateway.base_url))
        .header("Authorization", "Bearer invalid-token")
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer")
    );
}

#[tokio::test]
async fn gateway_rejects_caller_selected_work_directories() {
    let gateway = start_gateway(vec![principal("tenant-a", TOKEN_A)]).await;
    let mut config = sandbox_config();
    config.work_dir = Some(std::path::PathBuf::from("/workspace"));
    let error =
        match RemoteSandbox::connect(config, remote_client(&gateway.base_url, TOKEN_A)).await {
            Ok(_) => panic!("gateway must reject caller-selected work directories"),
            Err(error) => error,
        };
    assert!(error.to_string().contains("work_dir must be omitted"));
}

#[tokio::test]
async fn gateway_rejects_unsupported_denylists() {
    let gateway = start_gateway(vec![principal("tenant-a", TOKEN_A)]).await;
    let mut config = sandbox_config();
    config.fs_denylist = vec![std::path::PathBuf::from("/etc/passwd")];
    let error =
        match RemoteSandbox::connect(config, remote_client(&gateway.base_url, TOKEN_A)).await {
            Ok(_) => panic!("gateway must reject unsupported denylists"),
            Err(error) => error,
        };
    assert!(error.to_string().contains("fs_denylist is not supported"));
}

#[tokio::test]
async fn bwrap_gateway_supports_files_shell_reset_and_delete() {
    if !bwrap_available() {
        return;
    }
    let gateway = start_gateway(vec![principal("tenant-a", TOKEN_A)]).await;
    let sandbox = connect(&gateway, TOKEN_A).await;
    let work_dir = sandbox.env().working_dir().to_path_buf();
    assert!(work_dir.is_absolute());
    assert!(work_dir.exists());

    let file = work_dir.join("hello.txt");
    sandbox
        .env()
        .write_file(&file, b"first\n", CancellationToken::new())
        .await
        .expect("write file");
    let text = sandbox
        .env()
        .read_text_file(&file, CancellationToken::new())
        .await
        .expect("read file");
    assert_eq!(text, "first\n");

    let output = sandbox
        .env()
        .execute_shell(
            "printf gateway; printf 'shell'",
            shell_options(Duration::from_secs(5)),
        )
        .await
        .expect("execute shell");
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, "gatewayshell");

    let temp_dir = sandbox
        .env()
        .create_temp_dir("agent")
        .await
        .expect("temp dir");
    assert!(temp_dir.starts_with(work_dir.join(".tmp")));
    assert!(temp_dir.is_dir());
    sandbox.env().cleanup().await.expect("cleanup temp dirs");
    assert!(!work_dir.join(".tmp").exists());

    let outside = std::path::PathBuf::from("/etc/llm-harness-gateway-test");
    let error = sandbox
        .env()
        .write_file(&outside, b"forbidden", CancellationToken::new())
        .await
        .expect_err("filesystem boundary must be enforced");
    assert!(matches!(error, EnvError::PermissionDenied(_)));

    sandbox.reset().await.expect("reset sandbox");
    assert!(!file.exists());

    Box::new(sandbox)
        .shutdown()
        .await
        .expect("shutdown sandbox");
    assert!(!work_dir.exists());
}

#[tokio::test]
async fn bwrap_gateway_blocks_network_by_default() {
    if !bwrap_available() {
        return;
    }
    let gateway = start_gateway(vec![principal("tenant-a", TOKEN_A)]).await;
    let sandbox = connect(&gateway, TOKEN_A).await;
    let command = "bash -c 'echo test > /dev/tcp/127.0.0.1/9' && echo NETWORK_ALLOWED || echo NETWORK_BLOCKED";
    let output = sandbox
        .env()
        .execute_shell(command, shell_options(Duration::from_secs(5)))
        .await
        .expect("execute shell");
    assert!(
        output.stdout.contains("NETWORK_BLOCKED"),
        "network must be default-deny: {}",
        output.stdout
    );
    Box::new(sandbox)
        .shutdown()
        .await
        .expect("shutdown sandbox");
}

#[tokio::test]
async fn bwrap_gateway_enforces_shell_timeout() {
    if !bwrap_available() {
        return;
    }
    let gateway = start_gateway(vec![principal("tenant-a", TOKEN_A)]).await;
    let sandbox = connect(&gateway, TOKEN_A).await;
    let error = match sandbox
        .env()
        .execute_shell("sleep 2", shell_options(Duration::from_millis(100)))
        .await
    {
        Ok(_) => panic!("shell timeout must be enforced"),
        Err(error) => error,
    };
    assert!(matches!(error, EnvError::ShellTimeout { .. }));
    Box::new(sandbox)
        .shutdown()
        .await
        .expect("shutdown sandbox");
}

#[tokio::test]
async fn bwrap_gateway_aborts_running_shell_promptly() {
    if !bwrap_available() {
        return;
    }
    let gateway = start_gateway(vec![principal("tenant-a", TOKEN_A)]).await;
    let sandbox = connect(&gateway, TOKEN_A).await;
    let abort = CancellationToken::new();
    let options = shell_options_with_abort(Duration::from_secs(10), abort.clone());
    let execution =
        tokio::spawn(async move { sandbox.env().execute_shell("sleep 5", options).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = Instant::now();
    abort.cancel();
    let error = match execution.await.expect("execution task") {
        Ok(_) => panic!("running shell must be aborted"),
        Err(error) => error,
    };
    assert!(matches!(error, EnvError::ShellAborted { .. }));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "abort must reach the gateway before the command timeout: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn gateway_enforces_sandbox_capacity_under_concurrent_creation() {
    if !bwrap_available() {
        return;
    }
    let config = GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
        .expect("gateway config")
        .with_max_sandboxes(1);
    let gateway = start_gateway_with_config(config).await;
    let client = remote_client(&gateway.base_url, TOKEN_A);
    let first = client.create_sandbox(sandbox_config());
    let second = client.create_sandbox(sandbox_config());
    let (first, second) = tokio::join!(first, second);
    let first = first.expect("first sandbox");
    let error = second.expect_err("second sandbox must be rejected");
    assert!(error.to_string().contains("sandbox limit reached"));
    client
        .delete_sandbox(&first.sandbox_id)
        .await
        .expect("delete sandbox");
}

#[tokio::test]
async fn gateway_rejects_cross_tenant_sandbox_access() {
    if !bwrap_available() {
        return;
    }
    let gateway = start_gateway(vec![
        principal("tenant-a", TOKEN_A),
        principal("tenant-b", TOKEN_B),
    ])
    .await;
    let client_a = remote_client(&gateway.base_url, TOKEN_A);
    let response = client_a
        .create_sandbox(sandbox_config())
        .await
        .expect("create sandbox");
    let sandbox_id = response.sandbox_id;

    let response = reqwest::Client::new()
        .get(format!("{}/v1/sandboxes/{sandbox_id}", gateway.base_url))
        .header("Authorization", format!("Bearer {TOKEN_B}"))
        .send()
        .await
        .expect("request status");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);

    client_a
        .delete_sandbox(&sandbox_id)
        .await
        .expect("delete sandbox");
}
