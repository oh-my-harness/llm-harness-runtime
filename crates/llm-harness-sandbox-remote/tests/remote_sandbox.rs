mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use llm_harness_platform::sandbox::Sandbox;
use llm_harness_sandbox_remote::{RemoteSandbox, RemoteSandboxClient, RemoteSandboxClientConfig};
use llm_harness_types::{EnvError, ShellOptions};
use tokio_util::sync::CancellationToken;

async fn setup() -> (common::TestGateway, Arc<RemoteSandboxClient>, RemoteSandbox) {
    let gateway = common::TestGateway::start().await;
    let config = RemoteSandboxClientConfig::new(gateway.base_url.clone(), common::GATEWAY_TOKEN)
        .expect("valid client config");
    let client = Arc::new(RemoteSandboxClient::new(config).expect("build client"));
    let sandbox = RemoteSandbox::connect(common::test_sandbox_config(), Arc::clone(&client))
        .await
        .expect("connect remote sandbox");
    (gateway, client, sandbox)
}

async fn setup_with_read_delay(
    delay: Duration,
) -> (common::TestGateway, Arc<RemoteSandboxClient>, RemoteSandbox) {
    let gateway = common::TestGateway::start_with_read_delay(delay).await;
    let config = RemoteSandboxClientConfig::new(gateway.base_url.clone(), common::GATEWAY_TOKEN)
        .expect("valid client config");
    let client = Arc::new(RemoteSandboxClient::new(config).expect("build client"));
    let sandbox = RemoteSandbox::connect(common::test_sandbox_config(), Arc::clone(&client))
        .await
        .expect("connect remote sandbox");
    (gateway, client, sandbox)
}

#[tokio::test]
async fn remote_sandbox_supports_filesystem_and_streaming_shell() {
    let (_gateway, _client, sandbox) = setup().await;
    sandbox.start().await.expect("start sandbox");
    let env = sandbox.env();

    let file = sandbox_working_dir(&sandbox).join("hello.txt");
    env.write_file(&file, b"first\n", CancellationToken::new())
        .await
        .expect("write file");
    env.append_file(&file, b"second\n", CancellationToken::new())
        .await
        .expect("append file");

    let text = env
        .read_text_file(&file, CancellationToken::new())
        .await
        .expect("read text");
    assert_eq!(text, "first\nsecond\n");

    let lines = env
        .read_text_lines(&file, Some(1), CancellationToken::new())
        .await
        .expect("read lines");
    assert_eq!(lines, vec!["first".to_owned()]);

    let binary = env
        .read_binary_file(&file, CancellationToken::new())
        .await
        .expect("read binary");
    assert_eq!(binary, b"first\nsecond\n");

    let directory = sandbox_working_dir(&sandbox).join("nested");
    env.create_dir(&directory, true, CancellationToken::new())
        .await
        .expect("create directory");
    assert!(
        env.exists(&directory, CancellationToken::new())
            .await
            .unwrap()
    );

    let entries = env
        .list_dir(&sandbox_working_dir(&sandbox), CancellationToken::new())
        .await
        .expect("list directory");
    assert!(
        entries
            .iter()
            .any(|entry| entry.path.ends_with("hello.txt"))
    );

    let info = env
        .file_info(&file, CancellationToken::new())
        .await
        .expect("file info");
    assert_eq!(info.size, b"first\nsecond\n".len() as u64);

    let temp_dir = env.create_temp_dir("agent").await.expect("create temp dir");
    assert_eq!(
        temp_dir.parent(),
        Some(std::path::Path::new("/workspace/.tmp"))
    );
    assert!(
        temp_dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("agent-"))
    );

    let stdout_sender = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let stderr_sender = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let stdout_capture = std::sync::Arc::clone(&stdout_sender);
    let stderr_capture = std::sync::Arc::clone(&stderr_sender);
    let on_stdout = Box::new(move |chunk: &str| {
        stdout_capture.lock().unwrap().push(chunk.to_owned());
    });
    let on_stderr = Box::new(move |chunk: &str| {
        stderr_capture.lock().unwrap().push(chunk.to_owned());
    });
    let options = ShellOptions {
        cwd: None,
        env: vec![("REMOTE_TEST", "1")],
        timeout: Some(Duration::from_secs(2)),
        abort: CancellationToken::new(),
        on_stdout: Some(on_stdout),
        on_stderr: Some(on_stderr),
        max_output_bytes: Some(64 * 1024),
    };
    let output = env
        .execute_shell(
            "printf 'out'; printf 'err' >&2; test \"$REMOTE_TEST\" = 1",
            options,
        )
        .await
        .expect("execute shell");
    let stdout_chunks = stdout_sender.lock().unwrap().clone();
    let stderr_chunks = stderr_sender.lock().unwrap().clone();
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, "out");
    assert_eq!(output.stderr, "err");
    assert_eq!(stdout_chunks.join(""), "out");
    assert_eq!(stderr_chunks.join(""), "err");

    env.cleanup().await.expect("cleanup environment");
    sandbox.reset().await.expect("reset sandbox");
    assert!(!env.exists(&file, CancellationToken::new()).await.unwrap());
}

#[tokio::test]
async fn remote_environment_keeps_sandbox_alive_after_owner_drops() {
    let (gateway, client, sandbox) = setup().await;
    sandbox.start().await.expect("start sandbox");
    let env = sandbox.env();
    let sandbox_id = sandbox.sandbox_id().to_owned();
    drop(sandbox);

    let file = env.working_dir().join("alive.txt");
    env.write_file(&file, b"alive", CancellationToken::new())
        .await
        .expect("write file after owner drops");
    assert!(
        client
            .get_sandbox(&sandbox_id)
            .await
            .expect("remote sandbox should remain active")
            .state
            == llm_harness_sandbox_remote::protocol::RemoteSandboxState::Running
    );

    drop(env);
    let deadline = Instant::now() + Duration::from_secs(2);
    while gateway.delete_count() == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(gateway.delete_count(), 1);
}

#[tokio::test]
async fn remote_sandbox_enforces_allowlist() {
    let (_gateway, _client, sandbox) = setup().await;
    sandbox.start().await.expect("start sandbox");
    let env = sandbox.env();
    let result = env
        .write_file(
            std::path::Path::new("/etc/passwd"),
            b"forbidden",
            CancellationToken::new(),
        )
        .await;
    match result {
        Err(EnvError::PermissionDenied(path)) => {
            assert_eq!(path, std::path::PathBuf::from("/etc/passwd"))
        }
        other => panic!("expected permission denied, got {other:?}"),
    }
}

#[tokio::test]
async fn remote_sandbox_enforces_denylist() {
    let gateway = common::TestGateway::start().await;
    let config = RemoteSandboxClientConfig::new(gateway.base_url.clone(), common::GATEWAY_TOKEN)
        .expect("valid client config");
    let client = Arc::new(RemoteSandboxClient::new(config).expect("build client"));
    let mut sandbox_config = common::test_sandbox_config();
    sandbox_config.fs_denylist = vec![std::path::PathBuf::from("/workspace/secret")];
    let sandbox = RemoteSandbox::connect(sandbox_config, Arc::clone(&client))
        .await
        .expect("connect remote sandbox");
    sandbox.start().await.expect("start sandbox");
    let env = sandbox.env();
    let result = env
        .write_file(
            std::path::Path::new("/workspace/secret/value"),
            b"forbidden",
            CancellationToken::new(),
        )
        .await;
    match result {
        Err(EnvError::PermissionDenied(path)) => {
            assert_eq!(path, std::path::PathBuf::from("/workspace/secret/value"))
        }
        other => panic!("expected permission denied, got {other:?}"),
    }
}

#[tokio::test]
async fn remote_sandbox_maps_timeout_to_env_timeout() {
    let (_gateway, _client, sandbox) = setup().await;
    sandbox.start().await.expect("start sandbox");
    let env = sandbox.env();
    let options = ShellOptions {
        cwd: None,
        env: Vec::new(),
        timeout: Some(Duration::from_millis(100)),
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
        max_output_bytes: Some(64 * 1024),
    };
    let result = env.execute_shell("sleep 5", options).await;
    match result {
        Err(EnvError::ShellTimeout { .. }) => {}
        other => panic!("expected shell timeout, got success: {}", other.is_ok()),
    }
}

#[tokio::test]
async fn remote_sandbox_applies_client_timeout_cap_to_gateway_requests() {
    let gateway = common::TestGateway::start().await;
    let config = RemoteSandboxClientConfig::new(gateway.base_url.clone(), common::GATEWAY_TOKEN)
        .expect("valid client config")
        .with_max_exec_timeout(Duration::from_millis(100));
    let client = Arc::new(RemoteSandboxClient::new(config).expect("build client"));
    let sandbox = RemoteSandbox::connect(common::test_sandbox_config(), Arc::clone(&client))
        .await
        .expect("connect remote sandbox");
    sandbox.start().await.expect("start sandbox");
    let env = sandbox.env();
    let options = ShellOptions {
        cwd: None,
        env: Vec::new(),
        timeout: Some(Duration::from_secs(5)),
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
        max_output_bytes: Some(64 * 1024),
    };
    let result = env.execute_shell("sleep 5", options).await;
    match result {
        Err(EnvError::ShellTimeout { .. }) => {}
        other => panic!("expected shell timeout, got success: {}", other.is_ok()),
    }
}

#[tokio::test]
async fn remote_sandbox_uses_timeout_cap_when_no_timeout_is_requested() {
    let gateway = common::TestGateway::start().await;
    let config = RemoteSandboxClientConfig::new(gateway.base_url.clone(), common::GATEWAY_TOKEN)
        .expect("valid client config")
        .with_max_exec_timeout(Duration::from_millis(100));
    let client = Arc::new(RemoteSandboxClient::new(config).expect("build client"));
    let sandbox = RemoteSandbox::connect(common::test_sandbox_config(), Arc::clone(&client))
        .await
        .expect("connect remote sandbox");
    sandbox.start().await.expect("start sandbox");
    let env = sandbox.env();
    let options = ShellOptions {
        cwd: None,
        env: Vec::new(),
        timeout: None,
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
        max_output_bytes: Some(64 * 1024),
    };
    let result = env.execute_shell("sleep 5", options).await;
    match result {
        Err(EnvError::ShellTimeout { .. }) => {}
        other => panic!("expected shell timeout, got success: {}", other.is_ok()),
    }
}

#[tokio::test]
async fn remote_sandbox_bounds_final_shell_output() {
    let (_gateway, _client, sandbox) = setup().await;
    sandbox.start().await.expect("start sandbox");
    let env = sandbox.env();
    let options = ShellOptions {
        cwd: None,
        env: Vec::new(),
        timeout: Some(Duration::from_secs(2)),
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
        max_output_bytes: Some(6),
    };
    let output = env.execute_shell("printf 0123456789", options).await;
    let output = output.expect("execute bounded shell");
    assert_eq!(output.stdout, "012789");
    assert_eq!(output.stdout_truncated_bytes, 4);
}

#[tokio::test]
async fn remote_sandbox_maps_abort_to_shell_abort() {
    let (_gateway, _client, sandbox) = setup().await;
    sandbox.start().await.expect("start sandbox");
    let env = sandbox.env();
    let abort = CancellationToken::new();
    let abort_for_task = abort.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        abort_for_task.cancel();
    });
    let options = ShellOptions {
        cwd: None,
        env: Vec::new(),
        timeout: Some(Duration::from_secs(5)),
        abort,
        on_stdout: None,
        on_stderr: None,
        max_output_bytes: Some(64 * 1024),
    };
    let result = env.execute_shell("sleep 5", options).await;
    match result {
        Err(EnvError::ShellAborted { .. }) => {}
        other => panic!("expected shell abort, got success: {}", other.is_ok()),
    }
}

#[tokio::test]
async fn remote_sandbox_delete_removes_remote_state() {
    let (_gateway, client, sandbox) = setup().await;
    sandbox.start().await.expect("start sandbox");
    let sandbox_id = sandbox.sandbox_id().to_owned();
    Box::new(sandbox)
        .shutdown()
        .await
        .expect("shutdown sandbox");
    let error = client
        .get_sandbox(&sandbox_id)
        .await
        .expect_err("sandbox should be deleted");
    assert!(matches!(
        error,
        llm_harness_sandbox_remote::RemoteSandboxError::Gateway { status: 404, .. }
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(_gateway.delete_count(), 1);
}

#[tokio::test]
async fn invalid_create_response_still_deletes_remote_sandbox() {
    let gateway = common::TestGateway::start_with_create_work_dir("/outside".to_owned()).await;
    let config = RemoteSandboxClientConfig::new(gateway.base_url.clone(), common::GATEWAY_TOKEN)
        .expect("valid client config");
    let client = Arc::new(RemoteSandboxClient::new(config).expect("build client"));
    let result = RemoteSandbox::connect(common::test_sandbox_config(), Arc::clone(&client)).await;
    match result {
        Err(llm_harness_sandbox_remote::RemoteSandboxError::Protocol(_)) => {}
        other => panic!("expected protocol error, got success: {}", other.is_ok()),
    }
    assert_eq!(gateway.delete_count(), 1);
}

#[tokio::test]
async fn client_rejects_wrong_json_success_status() {
    let gateway =
        common::TestGateway::start_with_create_status(axum::http::StatusCode::ACCEPTED).await;
    let config = RemoteSandboxClientConfig::new(gateway.base_url.clone(), common::GATEWAY_TOKEN)
        .expect("valid client config");
    let client = RemoteSandboxClient::new(config).expect("build client");
    let error = client
        .create_sandbox(common::test_sandbox_config())
        .await
        .expect_err("wrong success status should be rejected");
    assert!(matches!(
        error,
        llm_harness_sandbox_remote::RemoteSandboxError::Protocol(_)
    ));
}

#[tokio::test]
async fn remote_file_request_can_be_cancelled_while_in_flight() {
    let (_gateway, _client, sandbox) = setup_with_read_delay(Duration::from_millis(200)).await;
    sandbox.start().await.expect("start sandbox");
    let env = sandbox.env();
    let path = std::path::Path::new("/workspace/input.txt");
    env.write_file(path, b"value", CancellationToken::new())
        .await
        .expect("write file");

    let abort = CancellationToken::new();
    let abort_for_task = abort.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        abort_for_task.cancel();
    });
    let result = env.read_text_file(path, abort).await;
    assert!(matches!(result, Err(EnvError::Aborted)), "{result:?}");
}

fn sandbox_working_dir(sandbox: &RemoteSandbox) -> std::path::PathBuf {
    sandbox.env().working_dir().to_path_buf()
}
