use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use llm_harness_platform::sandbox::{ResourceLimits, Sandbox, SandboxConfig};
use llm_harness_sandbox_gateway::{GatewayConfig, GatewayPrincipalConfig, SandboxGateway};
use llm_harness_sandbox_remote::protocol::RemoteSandboxState;
use llm_harness_sandbox_remote::{
    RemoteSandbox, RemoteSandboxClient, RemoteSandboxClientConfig, RemoteSandboxConfig,
    ResourceLimitsWire,
};
use llm_harness_types::{EnvError, ShellOptions};
use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

const TOKEN_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TOKEN_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct TestGateway {
    base_url: String,
    work_root: PathBuf,
    _gateway: SandboxGateway,
    _work_root: Option<tempfile::TempDir>,
    server: tokio::task::JoinHandle<()>,
}

async fn start_gateway(principals: Vec<GatewayPrincipalConfig>) -> TestGateway {
    start_gateway_with_config(GatewayConfig::new(principals).expect("gateway config")).await
}

async fn start_gateway_with_config(config: GatewayConfig) -> TestGateway {
    let work_root = private_tempdir();
    let path = work_root.path().to_path_buf();
    let mut gateway = start_gateway_at(config, path).await;
    gateway._work_root = Some(work_root);
    gateway
}

async fn start_gateway_at(config: GatewayConfig, work_root: PathBuf) -> TestGateway {
    let config = config.with_work_root(work_root.clone());
    let gateway = SandboxGateway::new(config).expect("gateway");
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind gateway");
    let address = listener.local_addr().expect("gateway address");
    let server_gateway = gateway.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, server_gateway.router())
            .await
            .expect("gateway server");
    });
    TestGateway {
        base_url: format!("http://{address}"),
        work_root,
        _gateway: gateway,
        _work_root: None,
        server,
    }
}

impl TestGateway {
    fn work_root(&self) -> &Path {
        &self.work_root
    }

    async fn stop(self) -> PathBuf {
        let TestGateway {
            base_url: _,
            work_root,
            _gateway,
            _work_root,
            server,
        } = self;
        server.abort();
        let _ = server.await;
        drop(_gateway);
        match _work_root {
            Some(work_root) => work_root.keep(),
            None => work_root,
        }
    }
}

fn principal(id: &str, token: &str) -> GatewayPrincipalConfig {
    GatewayPrincipalConfig {
        principal: id.to_owned(),
        bearer_token: token.to_owned(),
        fs_allowlist: Vec::new(),
    }
}

fn private_tempdir() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("gateway work root");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
        .expect("gateway work root permissions");
    directory
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

fn persisted_sandbox_config_json() -> String {
    serde_json::to_string(&RemoteSandboxConfig {
        fs_allowlist: Vec::new(),
        fs_denylist: Vec::new(),
        net_allowlist: Vec::new(),
        resource_limits: ResourceLimitsWire {
            max_cpus: None,
            max_processes: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout_ms: None,
        },
        work_dir: None,
    })
    .expect("serialize persisted sandbox config")
}

fn insert_test_registry_record(
    work_root: &Path,
    sandbox_id: &str,
    owner: &str,
    config_json: &str,
    state: &str,
) {
    let connection =
        Connection::open(work_root.join("registry.sqlite")).expect("open test registry");
    connection
        .execute(
            "INSERT INTO sandboxes
                 (sandbox_id, owner, config_json, state, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, 0, 0)",
            rusqlite::params![sandbox_id, owner, config_json, state],
        )
        .expect("insert test registry record");
}

fn registry_record_count(work_root: &Path) -> usize {
    let connection =
        Connection::open(work_root.join("registry.sqlite")).expect("open test registry");
    connection
        .query_row("SELECT count(*) FROM sandboxes", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count test registry records")
        .try_into()
        .expect("registry count fits usize")
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
async fn gateway_invalid_allowlist_does_not_leave_a_work_root() {
    let gateway = start_gateway(vec![principal("tenant-a", TOKEN_A)]).await;
    let mut config = sandbox_config();
    config.fs_allowlist = vec![std::path::PathBuf::from("/etc")];
    let error =
        match RemoteSandbox::connect(config, remote_client(&gateway.base_url, TOKEN_A)).await {
            Ok(_) => panic!("gateway must reject paths outside the principal allowlist"),
            Err(error) => error,
        };
    assert!(
        error
            .to_string()
            .contains("not permitted for this principal")
    );

    let sandbox_root = gateway.work_root().join("sandboxes");
    let entries = std::fs::read_dir(&sandbox_root)
        .expect("read gateway sandbox root")
        .count();
    assert_eq!(
        entries, 0,
        "invalid sandbox creation must not leave a work root"
    );
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
    assert!(work_dir.starts_with(gateway.work_root().join("sandboxes")));

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
async fn gateway_restores_sandbox_work_roots_and_ownership_after_restart() {
    if !bwrap_available() {
        return;
    }
    let first = start_gateway(vec![
        principal("tenant-a", TOKEN_A),
        principal("tenant-b", TOKEN_B),
    ])
    .await;
    let client = remote_client(&first.base_url, TOKEN_A);
    let response = client
        .create_sandbox(sandbox_config())
        .await
        .expect("create sandbox");
    let file = PathBuf::from(&response.work_dir).join("restart.txt");
    client
        .write_file(&response.sandbox_id, &file, b"persisted\n", false)
        .await
        .expect("write file before restart");
    assert!(file.exists());

    let work_root = first.stop().await;
    assert!(file.exists());

    let second = start_gateway_at(
        GatewayConfig::new(vec![
            principal("tenant-a", TOKEN_A),
            principal("tenant-b", TOKEN_B),
        ])
        .expect("gateway config"),
        work_root.clone(),
    )
    .await;
    let client_a = remote_client(&second.base_url, TOKEN_A);
    let status = client_a
        .get_sandbox(&response.sandbox_id)
        .await
        .expect("get restored sandbox");
    assert_eq!(status.state, RemoteSandboxState::Running);
    assert_eq!(status.work_dir, response.work_dir);
    assert_eq!(
        client_a
            .read_file(&response.sandbox_id, &file, false)
            .await
            .expect("read file after restart"),
        b"persisted\n"
    );

    let cross_tenant_response = reqwest::Client::new()
        .get(format!(
            "{}/v1/sandboxes/{}",
            second.base_url, response.sandbox_id
        ))
        .header("Authorization", format!("Bearer {TOKEN_B}"))
        .send()
        .await
        .expect("cross-tenant status request");
    assert_eq!(
        cross_tenant_response.status(),
        reqwest::StatusCode::NOT_FOUND
    );

    client_a
        .delete_sandbox(&response.sandbox_id)
        .await
        .expect("delete restored sandbox");
    assert!(!file.exists());
    assert_eq!(registry_record_count(&work_root), 0);

    let work_root = second.stop().await;
    std::fs::remove_dir_all(work_root).expect("remove restart work root");
}

#[test]
fn gateway_rejects_concurrent_work_root_owners() {
    let work_root = private_tempdir();
    let config = GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
        .expect("gateway config")
        .with_work_root(work_root.path().to_path_buf());
    let first = SandboxGateway::new(config.clone()).expect("first gateway");
    let error = match SandboxGateway::new(config) {
        Ok(_) => panic!("second gateway must not share a work root"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("already in use"));
    drop(first);
}

#[test]
fn gateway_recovers_orphan_work_roots_on_startup() {
    let work_root = private_tempdir();
    let config = GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
        .expect("gateway config")
        .with_work_root(work_root.path().to_path_buf());
    let first = SandboxGateway::new(config.clone()).expect("first gateway");
    drop(first);

    let sandbox_root = work_root.path().join("sandboxes");
    let orphan = sandbox_root.join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir(&orphan).expect("orphan root");
    std::fs::write(orphan.join("data.txt"), "orphan").expect("orphan file");
    let sentinel = work_root.path().join("sentinel");
    std::fs::write(&sentinel, "preserved").expect("sentinel file");
    std::os::unix::fs::symlink("../sentinel", orphan.join("escape")).expect("orphan symlink");

    let second = SandboxGateway::new(config).expect("second gateway");
    assert!(!orphan.exists());
    assert!(sentinel.exists(), "orphan cleanup must not follow symlinks");
    assert_eq!(
        std::fs::read_to_string(&sentinel).expect("read sentinel"),
        "preserved"
    );
    drop(second);
}

#[test]
fn gateway_resumes_deleting_registry_records_on_startup() {
    let work_root = private_tempdir();
    let config = GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
        .expect("gateway config")
        .with_work_root(work_root.path().to_path_buf());
    let first = SandboxGateway::new(config.clone()).expect("first gateway");
    drop(first);

    let sandbox_id = uuid::Uuid::new_v4().to_string();
    let sandbox_root = work_root.path().join("sandboxes");
    let sandbox_path = sandbox_root.join(&sandbox_id);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&sandbox_path)
        .expect("deleting sandbox work root");
    insert_test_registry_record(
        work_root.path(),
        &sandbox_id,
        "tenant-a",
        &persisted_sandbox_config_json(),
        "deleting",
    );

    let second = SandboxGateway::new(config).expect("second gateway");
    assert!(!sandbox_path.exists());
    assert_eq!(registry_record_count(work_root.path()), 0);
    drop(second);
}

#[test]
fn gateway_fails_closed_on_invalid_persisted_registry_entries() {
    let work_root = private_tempdir();
    let config = GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
        .expect("gateway config")
        .with_work_root(work_root.path().to_path_buf());
    let first = SandboxGateway::new(config.clone()).expect("first gateway");
    drop(first);

    let sandbox_id = uuid::Uuid::new_v4().to_string();
    let sandbox_path = work_root.path().join("sandboxes").join(&sandbox_id);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&sandbox_path)
        .expect("invalid sandbox work root");
    insert_test_registry_record(
        work_root.path(),
        &sandbox_id,
        "missing-owner",
        &persisted_sandbox_config_json(),
        "running",
    );

    let error = match SandboxGateway::new(config) {
        Ok(_) => panic!("gateway must reject an invalid persisted registry entry"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("unconfigured owner"));
    assert!(sandbox_path.exists());
}

#[test]
fn gateway_fails_closed_on_missing_persisted_work_root() {
    let work_root = private_tempdir();
    let config = GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
        .expect("gateway config")
        .with_work_root(work_root.path().to_path_buf());
    let first = SandboxGateway::new(config.clone()).expect("first gateway");
    drop(first);

    let sandbox_id = uuid::Uuid::new_v4().to_string();
    insert_test_registry_record(
        work_root.path(),
        &sandbox_id,
        "tenant-a",
        &persisted_sandbox_config_json(),
        "running",
    );

    let error = match SandboxGateway::new(config) {
        Ok(_) => panic!("gateway must reject a persisted sandbox without a work root"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("missing its work root"));
}

#[test]
fn gateway_fails_closed_on_corrupt_registry() {
    let work_root = private_tempdir();
    std::fs::write(
        work_root.path().join("registry.sqlite"),
        b"not a sqlite database",
    )
    .expect("write corrupt registry");
    std::fs::set_permissions(
        work_root.path().join("registry.sqlite"),
        std::fs::Permissions::from_mode(0o600),
    )
    .expect("corrupt registry permissions");
    let config = GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
        .expect("gateway config")
        .with_work_root(work_root.path().to_path_buf());
    let error = match SandboxGateway::new(config) {
        Ok(_) => panic!("gateway must reject a corrupt registry"),
        Err(error) => error,
    };
    assert!(
        error.to_string().to_lowercase().contains("database"),
        "unexpected corrupt registry error: {error}"
    );
}

#[test]
fn gateway_rejects_non_regular_work_root_lock_files() {
    let work_root = private_tempdir();
    let config = GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
        .expect("gateway config")
        .with_work_root(work_root.path().to_path_buf());
    let first = SandboxGateway::new(config.clone()).expect("first gateway");
    drop(first);

    let lock_path = work_root.path().join(".gateway.lock");
    std::fs::remove_file(&lock_path).expect("remove regular lock file");
    let path = std::ffi::CString::new(lock_path.as_os_str().as_encoded_bytes())
        .expect("lock path CString");
    let result = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
    assert_eq!(result, 0, "create FIFO lock file");

    let error = match SandboxGateway::new(config) {
        Ok(_) => panic!("gateway must reject a non-regular lock file"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("regular file"));
}

#[test]
fn gateway_rejects_unexpected_sandbox_root_entries() {
    let work_root = private_tempdir();
    let config = GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
        .expect("gateway config")
        .with_work_root(work_root.path().to_path_buf());
    let first = SandboxGateway::new(config.clone()).expect("first gateway");
    drop(first);

    let unexpected = work_root.path().join("sandboxes").join("unexpected");
    std::fs::create_dir(&unexpected).expect("unexpected entry");
    let error = match SandboxGateway::new(config) {
        Ok(_) => panic!("gateway must reject unexpected sandbox-root entries"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("unexpected entry"));
    assert!(
        unexpected.exists(),
        "invalid entry must be preserved for operator review"
    );
}

#[test]
fn gateway_rejects_work_root_with_wrong_permissions() {
    let work_root = private_tempdir();
    std::fs::set_permissions(work_root.path(), std::fs::Permissions::from_mode(0o755))
        .expect("work root permissions");
    let config = GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
        .expect("gateway config")
        .with_work_root(work_root.path().to_path_buf());
    let error = match SandboxGateway::new(config) {
        Ok(_) => panic!("gateway must reject a work root with wrong permissions"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("must have 0700 permissions"));
    std::fs::set_permissions(work_root.path(), std::fs::Permissions::from_mode(0o700))
        .expect("restore work root permissions");
}

#[test]
fn gateway_rejects_symlink_work_root() {
    let work_root = private_tempdir();
    let link_parent = private_tempdir();
    let link = link_parent.path().join("gateway");
    std::os::unix::fs::symlink(work_root.path(), &link).expect("work root symlink");
    let config = GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
        .expect("gateway config")
        .with_work_root(link);
    let error = match SandboxGateway::new(config) {
        Ok(_) => panic!("gateway must reject a symlink work root"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("must be a directory"));
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
