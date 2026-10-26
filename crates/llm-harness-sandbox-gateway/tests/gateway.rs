#![cfg(unix)]

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use llm_harness_platform::sandbox::NetRule;
use llm_harness_platform::sandbox::{ResourceLimits, Sandbox, SandboxConfig};
use llm_harness_sandbox_gateway::{
    FirecrackerBackendConfig, GatewayBackend, GatewayConfig, GatewayPrincipalConfig, SandboxGateway,
};
use llm_harness_sandbox_remote::protocol::{
    FileContent, RemoteSandboxState, UploadFile, UploadFilesRequest,
};
use llm_harness_sandbox_remote::{
    RemoteSandbox, RemoteSandboxClient, RemoteSandboxClientConfig, RemoteSandboxConfig,
    RemoteSandboxError, ResourceLimitsWire,
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
    // Bwrap is Linux-only; on macOS the default backend is unavailable, so
    // the shared behavior tests run against the Seatbelt backend instead.
    #[cfg(target_os = "macos")]
    let config = config.with_backend(GatewayBackend::Seatbelt);
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
        host_project_dirs: Vec::new(),
        max_trust_profile: llm_harness_platform::trust::TrustProfile::Restricted,
        requests_per_minute: None,
        max_active_sandboxes: None,
    }
}

fn private_tempdir() -> tempfile::TempDir {
    // macOS tempdir parents are symlinked (/var -> /private/var); the gateway
    // requires an already-canonical work root, so create the directory under
    // the canonicalized system temp path.
    let system_temp = std::fs::canonicalize(std::env::temp_dir()).expect("canonicalize temp");
    let directory = tempfile::tempdir_in(system_temp).expect("gateway work root");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
        .expect("gateway work root permissions");
    directory
}

#[test]
fn gateway_defaults_to_the_bwrap_backend() {
    let config = GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)]).expect("gateway config");
    assert!(matches!(config.backend(), GatewayBackend::Bwrap));
}

#[test]
fn gateway_rejects_zero_principal_sandbox_quota() {
    let mut principal = principal("tenant-a", TOKEN_A);
    principal.max_active_sandboxes = Some(0);

    let error = GatewayConfig::new(vec![principal]).expect_err("zero quota must be invalid");
    assert!(
        error
            .to_string()
            .contains("max_active_sandboxes for \"tenant-a\" must be positive"),
    );
}

#[tokio::test]
async fn gateway_enforces_per_principal_active_sandbox_quota() {
    let mut capped = principal("tenant-a", TOKEN_A);
    capped.max_active_sandboxes = Some(1);
    let first = start_gateway_with_config(
        GatewayConfig::new(vec![capped, principal("tenant-b", TOKEN_B)])
            .expect("gateway config")
            .with_max_sandboxes(2),
    )
    .await;
    let client_a = remote_client(&first.base_url, TOKEN_A);
    let client_b = remote_client(&first.base_url, TOKEN_B);

    let first_sandbox = client_a
        .create_sandbox(sandbox_config())
        .await
        .expect("create first sandbox for capped principal");
    let quota_error = client_a
        .create_sandbox(sandbox_config())
        .await
        .expect_err("per-principal quota must reject the second sandbox");
    assert!(quota_error.to_string().contains("sandbox limit reached"));

    client_b
        .create_sandbox(sandbox_config())
        .await
        .expect("another principal is independent of the capped principal");

    client_a
        .delete_sandbox(&first_sandbox.sandbox_id)
        .await
        .expect("delete sandbox");
    client_a
        .create_sandbox(sandbox_config())
        .await
        .expect("deleted sandbox releases principal capacity");

    let work_root = first.stop().await;
    std::fs::remove_dir_all(work_root).expect("remove work root");
}

#[cfg(target_os = "macos")]
#[test]
fn gateway_accepts_seatbelt_backend() {
    let work_root = private_tempdir();
    let gateway = SandboxGateway::new(
        GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
            .expect("gateway config")
            .with_work_root(work_root.path().to_path_buf())
            .with_backend(GatewayBackend::Seatbelt),
    );
    assert!(gateway.is_ok(), "seatbelt gateway must construct on macOS");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn seatbelt_gateway_enforces_allowlists_and_rejects_denylists() {
    let gateway = start_gateway(vec![principal("tenant-a", TOKEN_A)]).await;
    let client = remote_client(&gateway.base_url, TOKEN_A);

    // denylist: Seatbelt fails closed at construction time
    let mut denied = sandbox_config();
    denied.fs_denylist = vec![std::path::PathBuf::from("/workspace/denied")];
    let error = client
        .create_sandbox(denied)
        .await
        .expect_err("seatbelt must reject denylist policies");
    assert!(
        error.to_string().contains("fs_denylist is not supported"),
        "unexpected denylist error: {error}"
    );

    // allowlist: guest-coordinate entries map into the sandbox work dir
    let mut allowed = sandbox_config();
    allowed.fs_allowlist = vec![std::path::PathBuf::from("/workspace/allowed")];
    let sandbox = RemoteSandbox::connect(allowed, Arc::clone(&client))
        .await
        .expect("create sandbox with allowlist");
    let env = sandbox.env();
    let outside = std::path::PathBuf::from("/workspace/outside.txt");
    let error = env
        .write_file(&outside, b"forbidden\n", CancellationToken::new())
        .await
        .expect_err("allowlist must restrict guest paths");
    assert!(matches!(error, EnvError::PermissionDenied(_)));
    let inside = std::path::PathBuf::from("/workspace/allowed/file.txt");
    env.write_file(&inside, b"allowed\n", CancellationToken::new())
        .await
        .expect("write allowed path");
    Box::new(sandbox)
        .shutdown()
        .await
        .expect("shutdown sandbox");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn trusted_lan_binds_host_project_dir_and_caps_by_principal() {
    let project = private_tempdir();
    std::fs::write(project.path().join("marker.txt"), b"from-host").expect("write marker");
    let project_name = project
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let mut trusted = principal("tenant-a", TOKEN_A);
    trusted.host_project_dirs = vec![project.path().to_path_buf()];
    trusted.max_trust_profile = llm_harness_platform::trust::TrustProfile::TrustedLan;
    let mut kiosk = principal("kiosk", TOKEN_B);
    kiosk.max_trust_profile = llm_harness_platform::trust::TrustProfile::Restricted;
    let gateway = start_gateway(vec![trusted, kiosk]).await;
    let client_a = remote_client(&gateway.base_url, TOKEN_A);
    let client_b = remote_client(&gateway.base_url, TOKEN_B);

    // 1. trusted-lan create succeeds; the project dir is visible at
    //    /workspace/<basename> and read-write in both directions.
    let mut request = sandbox_config();
    request.trust_profile = llm_harness_platform::trust::TrustProfile::TrustedLan;
    let sandbox = RemoteSandbox::connect(request.clone(), Arc::clone(&client_a))
        .await
        .expect("trusted-lan create");
    let env = sandbox.env();
    let marker = std::path::PathBuf::from(format!("/workspace/{project_name}/marker.txt"));
    let text = env
        .read_text_file(&marker, CancellationToken::new())
        .await
        .expect("read bound host file");
    assert_eq!(text, "from-host");
    env.write_file(
        &std::path::PathBuf::from(format!("/workspace/{project_name}/from-guest.txt")),
        b"from-guest",
        CancellationToken::new(),
    )
    .await
    .expect("write into bound dir");
    assert_eq!(
        std::fs::read(project.path().join("from-guest.txt")).expect("host sees guest write"),
        b"from-guest"
    );
    Box::new(sandbox).shutdown().await.expect("shutdown");

    // 2. principal capped at restricted cannot request trusted-lan.
    let error = client_b
        .create_sandbox(request.clone())
        .await
        .expect_err("over-cap profile must be rejected");
    assert!(
        error.to_string().contains("exceeds principal cap"),
        "unexpected cap error: {error}"
    );

    // 3. a principal WITHOUT host_project_dirs: trusted-lan create still
    //    succeeds but nothing is bound (empty binds are a no-op).
    let mut empty = principal("tenant-c", TOKEN_A);
    empty.max_trust_profile = llm_harness_platform::trust::TrustProfile::TrustedFull;
    let _ = empty; // covered implicitly by tenant-a in other tests
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn trusted_lan_without_project_dirs_is_valid() {
    let mut p = principal("tenant-a", TOKEN_A);
    p.max_trust_profile = llm_harness_platform::trust::TrustProfile::TrustedFull;
    let gateway = start_gateway(vec![p]).await;
    let client = remote_client(&gateway.base_url, TOKEN_A);
    let mut request = sandbox_config();
    request.trust_profile = llm_harness_platform::trust::TrustProfile::TrustedLan;
    let sandbox = RemoteSandbox::connect(request, Arc::clone(&client))
        .await
        .expect("trusted-lan without binds is valid");
    Box::new(sandbox).shutdown().await.expect("shutdown");
}

#[test]
fn gateway_validates_a_firecracker_backend_at_construction() {
    let root = private_tempdir();
    for name in ["firecracker", "vmlinux", "rootfs-base.ext4"] {
        std::fs::write(root.path().join(name), b"artifact").expect("write Firecracker artifact");
    }
    let mut permissions = std::fs::metadata(root.path().join("firecracker"))
        .expect("binary metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(root.path().join("firecracker"), permissions)
        .expect("make Firecracker binary executable");

    let backend = firecracker_backend(root.path(), 3);
    let work_root = private_tempdir();
    let _gateway = SandboxGateway::new(
        GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
            .expect("gateway config")
            .with_work_root(work_root.path().to_path_buf())
            .with_backend(backend),
    )
    .expect("valid Firecracker gateway");
}

#[test]
fn gateway_rejects_a_relative_firecracker_path() {
    let backend = GatewayBackend::Firecracker(FirecrackerBackendConfig {
        binary_path: PathBuf::from("firecracker"),
        kernel_image_path: PathBuf::from("/vmlinux"),
        base_image_path: PathBuf::from("/rootfs.ext4"),
        max_vcpu_count: 1,
        max_memory_mib: 128,
        max_disk_mib: None,
        vsock_guest_cid_base: 3,
        guest_agent_port: 6000,
        readiness_timeout: Duration::from_secs(1),
    });

    let work_root = private_tempdir();
    let error = match SandboxGateway::new(
        GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
            .expect("gateway config")
            .with_work_root(work_root.path().to_path_buf())
            .with_backend(backend),
    ) {
        Ok(_) => panic!("relative Firecracker binary accepted"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("binary path must be absolute"));
}

#[test]
fn gateway_rejects_vsock_cid_capacity_overflow() {
    let root = private_tempdir();
    for name in ["firecracker", "vmlinux", "rootfs-base.ext4"] {
        std::fs::write(root.path().join(name), b"artifact").expect("write Firecracker artifact");
    }
    let mut permissions = std::fs::metadata(root.path().join("firecracker"))
        .expect("binary metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(root.path().join("firecracker"), permissions)
        .expect("make Firecracker binary executable");

    let backend = firecracker_backend(root.path(), u32::MAX);

    let work_root = private_tempdir();
    let error = match SandboxGateway::new(
        GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
            .expect("gateway config")
            .with_work_root(work_root.path().to_path_buf())
            .with_max_sandboxes(2)
            .with_backend(backend),
    ) {
        Ok(_) => panic!("Firecracker CID capacity overflow accepted"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("vsock CID capacity"));
}

#[test]
fn gateway_rejects_zero_firecracker_disk_capacity() {
    let root = private_tempdir();
    for name in ["firecracker", "vmlinux", "rootfs-base.ext4"] {
        std::fs::write(root.path().join(name), b"artifact").expect("write Firecracker artifact");
    }
    let mut permissions = std::fs::metadata(root.path().join("firecracker"))
        .expect("binary metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(root.path().join("firecracker"), permissions)
        .expect("make Firecracker binary executable");

    let backend = GatewayBackend::Firecracker(FirecrackerBackendConfig {
        binary_path: root.path().join("firecracker"),
        kernel_image_path: root.path().join("vmlinux"),
        base_image_path: root.path().join("rootfs-base.ext4"),
        max_vcpu_count: 1,
        max_memory_mib: 128,
        max_disk_mib: Some(0),
        vsock_guest_cid_base: 3,
        guest_agent_port: 6000,
        readiness_timeout: Duration::from_secs(1),
    });

    let work_root = private_tempdir();
    let error = match SandboxGateway::new(
        GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
            .expect("gateway config")
            .with_work_root(work_root.path().to_path_buf())
            .with_backend(backend),
    ) {
        Ok(_) => panic!("zero Firecracker disk capacity accepted"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("max_disk_mib must be positive"));
}

#[test]
fn gateway_rejects_firecracker_base_image_larger_than_disk_capacity() {
    let root = private_tempdir();
    std::fs::write(root.path().join("firecracker"), b"artifact")
        .expect("write Firecracker artifact");
    std::fs::write(root.path().join("vmlinux"), b"artifact").expect("write kernel artifact");
    std::fs::write(
        root.path().join("rootfs-base.ext4"),
        vec![0u8; 1024 * 1024 + 1],
    )
    .expect("write oversized Firecracker base image");
    let mut permissions = std::fs::metadata(root.path().join("firecracker"))
        .expect("binary metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(root.path().join("firecracker"), permissions)
        .expect("make Firecracker binary executable");

    let backend = GatewayBackend::Firecracker(FirecrackerBackendConfig {
        binary_path: root.path().join("firecracker"),
        kernel_image_path: root.path().join("vmlinux"),
        base_image_path: root.path().join("rootfs-base.ext4"),
        max_vcpu_count: 1,
        max_memory_mib: 128,
        max_disk_mib: Some(1),
        vsock_guest_cid_base: 3,
        guest_agent_port: 6000,
        readiness_timeout: Duration::from_secs(1),
    });

    let work_root = private_tempdir();
    let error = match SandboxGateway::new(
        GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
            .expect("gateway config")
            .with_work_root(work_root.path().to_path_buf())
            .with_backend(backend),
    ) {
        Ok(_) => panic!("Firecracker base image larger than disk capacity accepted"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("exceeds max_disk_mib"));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn firecracker_backend_does_not_fall_back_to_bwrap() {
    let root = private_tempdir();
    for name in ["firecracker", "vmlinux", "rootfs-base.ext4"] {
        std::fs::write(root.path().join(name), b"artifact").expect("write Firecracker artifact");
    }
    let mut permissions = std::fs::metadata(root.path().join("firecracker"))
        .expect("binary metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(root.path().join("firecracker"), permissions)
        .expect("make Firecracker binary executable");
    let work_root = private_tempdir();
    let gateway = start_gateway_with_config(
        GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
            .expect("gateway config")
            .with_work_root(work_root.path().to_path_buf())
            .with_backend(firecracker_backend(root.path(), 3)),
    )
    .await;

    let error =
        match RemoteSandbox::connect(sandbox_config(), remote_client(&gateway.base_url, TOKEN_A))
            .await
        {
            Ok(_) => panic!("Firecracker backend must not fall back to Bwrap"),
            Err(error) => error,
        };
    assert!(
        error
            .to_string()
            .contains("firecracker lifecycle startup failed")
    );
    assert_eq!(
        std::fs::read_dir(work_root.path().join("sandboxes"))
            .map(|entries| entries.count())
            .unwrap_or(0),
        0
    );
}

fn firecracker_backend(root: &Path, vsock_guest_cid_base: u32) -> GatewayBackend {
    GatewayBackend::Firecracker(FirecrackerBackendConfig {
        binary_path: root.join("firecracker"),
        kernel_image_path: root.join("vmlinux"),
        base_image_path: root.join("rootfs-base.ext4"),
        max_vcpu_count: 2,
        max_memory_mib: 256,
        max_disk_mib: Some(64),
        vsock_guest_cid_base,
        guest_agent_port: 6000,
        readiness_timeout: Duration::from_secs(2),
    })
}

#[cfg(target_os = "linux")]
fn firecracker_artifact_root() -> tempfile::TempDir {
    let root = private_tempdir();
    for name in ["firecracker", "vmlinux", "rootfs-base.ext4"] {
        std::fs::write(root.path().join(name), b"artifact").expect("write Firecracker artifact");
    }
    let mut permissions = std::fs::metadata(root.path().join("firecracker"))
        .expect("binary metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(root.path().join("firecracker"), permissions)
        .expect("make Firecracker binary executable");
    root
}

#[cfg(target_os = "linux")]
fn write_fake_firecracker(path: &Path, guest_port: u32) {
    let script = r#"#!/usr/bin/env python3
import base64, json, os, select, socket, sys
args = sys.argv[1:]
api_path = args[args.index("--api-sock") + 1]
vsock_path = os.path.join(os.path.dirname(api_path), "guest-agent.sock")
api_server = socket.socket(socket.AF_UNIX)
api_server.bind(api_path)
api_server.listen(4)
vsock_server = socket.socket(socket.AF_UNIX)
vsock_server.bind(vsock_path)
vsock_server.listen(4)
connections = []
files = {}

def recv_exact(connection, size):
    data = bytearray()
    while len(data) < size:
        chunk = connection.recv(size - len(data))
        if not chunk:
            raise EOFError("guest agent connection closed")
        data.extend(chunk)
    return bytes(data)

def read_frame(connection):
    header = recv_exact(connection, 20)
    payload_len = int.from_bytes(header[8:12], "big")
    payload = recv_exact(connection, payload_len)
    return int.from_bytes(header[12:20], "big"), json.loads(payload)

def frame(request_id, message):
    payload = json.dumps(message, separators=(",", ":")).encode()
    return (
        b"LHSV\x01\x00\x00\x00"
        + len(payload).to_bytes(4, "big")
        + request_id.to_bytes(8, "big")
        + payload
    )

def handle_guest_connection(connection):
    request_id, message = read_frame(connection)
    kind = message.get("type")
    data = message.get("data", {})
    if kind == "ping":
        message["type"] = "pong"
    elif kind == "cleanup":
        message["type"] = "cleanup_result"
    elif kind == "write_file":
        content = data["content"]
        files[data["path"]] = base64.b64decode(content["base64"]).decode()
        message = {"type": "write_file_result"}
    elif kind == "read_file":
        if data["path"] not in files:
            message = {"type": "error", "data": {"code": "not_found", "message": "missing"}}
        else:
            message = {
                "type": "read_file_result",
                "data": {"content": {"kind": "text", "value": files[data["path"]]}},
            }
    elif kind == "list_dir":
        message = {
            "type": "list_dir_result",
            "data": {"entries": [
                {"path": path, "is_dir": False, "size": len(value), "modified_ms": 0}
                for path, value in sorted(files.items())
            ]},
        }
    elif kind == "start_process":
        connection.sendall(frame(request_id, {
            "type": "process_started",
            "data": {"execution_id": "exec-1", "pid": 42},
        }))
        for name, value in sorted(files.items()):
            if name in data["command"]:
                connection.sendall(frame(request_id, {
                    "type": "process_stdout",
                    "data": {"execution_id": "exec-1", "chunk": value},
                }))
        connection.sendall(frame(request_id, {
            "type": "process_exit",
            "data": {"execution_id": "exec-1", "result": {
                "exit_code": 0, "stdout_truncated_bytes": 0, "stderr_truncated_bytes": 0,
            }},
        }))
        return
    else:
        return
    connection.sendall(frame(request_id, message))

while True:
    readable, _, _ = select.select([api_server, vsock_server, *connections], [], [])
    for source in readable:
        if source in connections:
            try:
                handle_guest_connection(source)
            except EOFError:
                connections.remove(source)
                source.close()
            continue
        connection, _ = source.accept()
        if source is api_server:
            request = connection.recv(4096)
            if request.startswith(b"PUT /actions"):
                connection.sendall(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                connection.close()
                sys.exit(0)
            connection.close()
            continue
        handshake = bytearray()
        while not handshake.endswith(b"\n"):
            handshake.extend(connection.recv(1))
        assert handshake == b"connect {guest_port}\n"
        connection.sendall(b"OK 1073741824\n")
        connections.append(connection)
"#;
    let script = script.replace("connect {guest_port}", &format!("connect {guest_port}"));
    std::fs::write(path, script).expect("write fake Firecracker");
    let mut permissions = std::fs::metadata(path)
        .expect("fake Firecracker metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("make fake Firecracker executable");
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
        trust_profile: Default::default(),
        host_binds: Vec::new(),
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
        trust_profile: Default::default(),
    })
    .expect("serialize persisted sandbox config")
}
#[test]
fn gateway_migrates_registry_schema_v1_to_v2() {
    let work_root = private_tempdir();
    let sandbox_root = work_root.path().join("sandboxes");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&sandbox_root)
        .expect("create sandbox root");
    let sandbox_id = uuid::Uuid::new_v4().to_string();
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(sandbox_root.join(&sandbox_id))
        .expect("create persisted sandbox root");

    let connection =
        Connection::open(work_root.path().join("registry.sqlite")).expect("create v1 registry");
    std::fs::set_permissions(
        work_root.path().join("registry.sqlite"),
        std::fs::Permissions::from_mode(0o600),
    )
    .expect("set v1 registry permissions");
    let config_json = persisted_sandbox_config_json();
    connection
        .execute_batch(&format!(
            "PRAGMA application_id = 1279808327;
             PRAGMA user_version = 1;
             CREATE TABLE sandboxes (
                 sandbox_id TEXT PRIMARY KEY,
                 owner TEXT NOT NULL,
                 config_json TEXT NOT NULL,
                 state TEXT NOT NULL CHECK (state IN ('creating', 'running', 'deleting')),
                 created_at_ms INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL
             );
             INSERT INTO sandboxes
                 (sandbox_id, owner, config_json, state, created_at_ms, updated_at_ms)
             VALUES ('{sandbox_id}', 'tenant-a', '{config_json}', 'deleting', 0, 0);",
        ))
        .expect("create v1 registry schema");

    let config = GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
        .expect("gateway config")
        .with_work_root(work_root.path().to_path_buf());
    let gateway = SandboxGateway::new(config).expect("migrate registry");
    drop(gateway);

    let connection =
        Connection::open(work_root.path().join("registry.sqlite")).expect("open migrated registry");
    let application_id: i64 = connection
        .query_row("PRAGMA application_id", [], |row| row.get(0))
        .expect("read application id");
    let schema_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read schema version");
    let columns: Vec<String> = {
        let mut statement = connection
            .prepare("PRAGMA table_info(sandboxes)")
            .expect("read table schema");
        statement
            .query_map([], |row| row.get(1))
            .expect("query table schema")
            .collect::<Result<_, _>>()
            .expect("read table columns")
    };
    assert_eq!(application_id, 0x4c_48_53_47);
    assert_eq!(schema_version, 2);
    assert_eq!(columns.last().map(String::as_str), Some("vsock_guest_cid"));
    assert_eq!(registry_record_count(work_root.path()), 0);
}

#[test]
fn gateway_registry_requires_unique_vsock_cids() {
    let work_root = private_tempdir();
    let config = GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
        .expect("gateway config")
        .with_work_root(work_root.path().to_path_buf());
    let gateway = SandboxGateway::new(config).expect("gateway");
    drop(gateway);

    let connection =
        Connection::open(work_root.path().join("registry.sqlite")).expect("open registry");
    let insert = |sandbox_id: &str, cid: Option<i64>| {
        connection
            .execute(
                "INSERT INTO sandboxes
                     (sandbox_id, owner, config_json, state, vsock_guest_cid, created_at_ms, updated_at_ms)
                 VALUES (?1, 'tenant-a', '{}', 'creating', ?2, 0, 0)",
                rusqlite::params![sandbox_id, cid],
            )
            .expect("insert first CID record");
    };
    insert(&uuid::Uuid::new_v4().to_string(), Some(3));
    let error = connection
        .execute(
            "INSERT INTO sandboxes
                 (sandbox_id, owner, config_json, state, vsock_guest_cid, created_at_ms, updated_at_ms)
             VALUES (?1, 'tenant-a', '{}', 'creating', 3, 0, 0)",
            rusqlite::params![uuid::Uuid::new_v4().to_string()],
        )
        .expect_err("duplicate vsock CID must be rejected");
    assert!(error.to_string().contains("UNIQUE"));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn firecracker_backend_rejects_unsupported_sandbox_policies() {
    let artifacts = firecracker_artifact_root();
    let work_root = private_tempdir();
    let gateway = start_gateway_with_config(
        GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
            .expect("gateway config")
            .with_work_root(work_root.path().to_path_buf())
            .with_backend(firecracker_backend(artifacts.path(), 3)),
    )
    .await;

    let unsupported = [
        (
            "fs_allowlist",
            SandboxConfig {
                fs_allowlist: vec![PathBuf::from("/workspace/allowed")],
                ..sandbox_config()
            },
        ),
        (
            "fs_denylist",
            SandboxConfig {
                fs_denylist: vec![PathBuf::from("/workspace/denied")],
                ..sandbox_config()
            },
        ),
        (
            "net_allowlist",
            SandboxConfig {
                net_allowlist: vec![NetRule {
                    host: "example.com".to_owned(),
                    port_range: None,
                }],
                ..sandbox_config()
            },
        ),
        (
            "max_processes",
            SandboxConfig {
                resource_limits: ResourceLimits {
                    max_processes: Some(16),
                    ..sandbox_config().resource_limits
                },
                ..sandbox_config()
            },
        ),
        (
            "max_disk_mb",
            SandboxConfig {
                resource_limits: ResourceLimits {
                    max_disk_mb: Some(1024),
                    ..sandbox_config().resource_limits
                },
                ..sandbox_config()
            },
        ),
        (
            "exceeds backend maximum",
            SandboxConfig {
                resource_limits: ResourceLimits {
                    max_cpus: Some(3),
                    max_memory_mb: Some(512),
                    ..sandbox_config().resource_limits
                },
                ..sandbox_config()
            },
        ),
    ];
    for (label, config) in unsupported {
        let error =
            match RemoteSandbox::connect(config, remote_client(&gateway.base_url, TOKEN_A)).await {
                Ok(_) => panic!("unsupported Firecracker policy {label} must be rejected"),
                Err(error) => error,
            };
        assert!(
            error.to_string().contains(label),
            "unexpected error for {label}: {error}"
        );
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn firecracker_gateway_start_reset_and_shutdown() {
    let artifacts = firecracker_artifact_root();
    write_fake_firecracker(artifacts.path().join("firecracker").as_path(), 6000);
    let gateway = start_gateway_with_config(
        GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
            .expect("gateway config")
            .with_backend(firecracker_backend(artifacts.path(), 3)),
    )
    .await;

    let sandbox =
        RemoteSandbox::connect(sandbox_config(), remote_client(&gateway.base_url, TOKEN_A))
            .await
            .expect("start Firecracker sandbox");
    assert_eq!(sandbox.backend(), "linux-firecracker");

    let connection =
        Connection::open(gateway.work_root().join("registry.sqlite")).expect("open registry");
    let cid: i64 = connection
        .query_row("SELECT vsock_guest_cid FROM sandboxes", [], |row| {
            row.get(0)
        })
        .expect("read persisted CID");
    assert_eq!(cid, 3);

    Sandbox::reset(&sandbox)
        .await
        .expect("reset Firecracker sandbox");
    Sandbox::shutdown(Box::new(sandbox))
        .await
        .expect("shut down Firecracker sandbox");
    assert_eq!(
        std::fs::read_dir(gateway.work_root().join("sandboxes"))
            .map(|entries| entries.count())
            .unwrap_or(0),
        0
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn firecracker_gateway_supports_shell_and_files() {
    let artifacts = firecracker_artifact_root();
    write_fake_firecracker(artifacts.path().join("firecracker").as_path(), 6000);
    let gateway = start_gateway_with_config(
        GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
            .expect("gateway config")
            .with_backend(firecracker_backend(artifacts.path(), 3)),
    )
    .await;

    let sandbox =
        RemoteSandbox::connect(sandbox_config(), remote_client(&gateway.base_url, TOKEN_A))
            .await
            .expect("start Firecracker sandbox");
    let file = PathBuf::from("/workspace/hello.txt");
    sandbox
        .env()
        .write_file(&file, b"persisted", CancellationToken::new())
        .await
        .expect("write guest file");
    assert_eq!(
        sandbox
            .env()
            .read_text_file(&file, CancellationToken::new())
            .await
            .expect("read guest file"),
        "persisted"
    );
    let entries = sandbox
        .env()
        .list_dir(Path::new("/workspace"), CancellationToken::new())
        .await
        .expect("list guest directory");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, file);

    let output = sandbox
        .env()
        .execute_shell(
            "cat /workspace/hello.txt",
            shell_options(Duration::from_secs(5)),
        )
        .await
        .expect("execute guest shell");
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, "persisted");

    Sandbox::shutdown(Box::new(sandbox))
        .await
        .expect("shut down Firecracker sandbox");
    assert_eq!(
        std::fs::read_dir(gateway.work_root().join("sandboxes"))
            .expect("read sandbox root")
            .count(),
        0
    );
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
async fn gateway_accepts_only_the_stable_guest_work_directory() {
    let gateway = start_gateway(vec![principal("tenant-a", TOKEN_A)]).await;
    let client = remote_client(&gateway.base_url, TOKEN_A);
    let default = client.create_sandbox(sandbox_config()).await;
    let default = default.expect("create sandbox with omitted work_dir");
    assert_eq!(default.work_dir, "/workspace");
    client
        .delete_sandbox(&default.sandbox_id)
        .await
        .expect("delete default sandbox");

    let mut config = sandbox_config();
    config.work_dir = Some(std::path::PathBuf::from("/workspace"));
    let stable = client.create_sandbox(config).await;
    let stable = stable.expect("create sandbox with stable work_dir");
    assert_eq!(stable.work_dir, "/workspace");
    client
        .delete_sandbox(&stable.sandbox_id)
        .await
        .expect("delete stable sandbox");

    let mut config = sandbox_config();
    config.work_dir = Some(std::path::PathBuf::from("/outside"));
    let error = client
        .create_sandbox(config)
        .await
        .expect_err("gateway must reject non-stable work directories");
    assert!(
        error
            .to_string()
            .contains("work_dir must be omitted or /workspace")
    );
}

fn work_root_contains_file(work_root: &Path, file_name: &str) -> bool {
    std::fs::read_dir(work_root.join("sandboxes"))
        .expect("read gateway sandbox root")
        .any(|entry| {
            entry
                .expect("sandbox entry")
                .path()
                .join(file_name)
                .exists()
        })
}

// fs_denylist enforcement is a Bwrap capability; Seatbelt fails closed on
// denylist policies by design (see llm-harness-sandbox seatbelt.rs).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn gateway_enforces_guest_denylists() {
    let gateway = start_gateway(vec![principal("tenant-a", TOKEN_A)]).await;
    let client = remote_client(&gateway.base_url, TOKEN_A);
    let mut config = sandbox_config();
    config.fs_denylist = vec![std::path::PathBuf::from("/workspace/denied")];
    let sandbox = RemoteSandbox::connect(config, Arc::clone(&client))
        .await
        .expect("create sandbox with denylist");
    let env = sandbox.env();
    let denied = std::path::PathBuf::from("/workspace/denied/file.txt");
    let error = env
        .write_file(&denied, b"forbidden\n", CancellationToken::new())
        .await
        .expect_err("denylist must take priority");
    assert!(matches!(error, EnvError::PermissionDenied(_)));
    let allowed = std::path::PathBuf::from("/workspace/allowed.txt");
    env.write_file(&allowed, b"allowed\n", CancellationToken::new())
        .await
        .expect("write allowed path");
    if bwrap_available() {
        let error = match sandbox
            .env()
            .execute_shell(
                "mkdir -p /workspace/denied && printf pwned > /workspace/denied/file",
                shell_options(Duration::from_secs(5)),
            )
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("denied shell path must fail"),
        };
        assert!(
            error.to_string().contains("shell_failed"),
            "unexpected denied shell error: {error:?}"
        );
    }
    let sibling = std::path::PathBuf::from("/workspace/denied-sibling/file.txt");
    env.write_file(&sibling, b"sibling\n", CancellationToken::new())
        .await
        .expect("write sibling outside denied prefix");
    Box::new(sandbox)
        .shutdown()
        .await
        .expect("shutdown sandbox");
}

#[tokio::test]
async fn gateway_enforces_guest_allowlists() {
    let gateway = start_gateway(vec![principal("tenant-a", TOKEN_A)]).await;
    let client = remote_client(&gateway.base_url, TOKEN_A);
    let mut config = sandbox_config();
    config.fs_allowlist = vec![std::path::PathBuf::from("/workspace/allowed")];
    let sandbox = RemoteSandbox::connect(config, Arc::clone(&client))
        .await
        .expect("create sandbox with allowlist");
    let env = sandbox.env();
    let outside = std::path::PathBuf::from("/workspace/outside.txt");
    let error = env
        .write_file(&outside, b"forbidden\n", CancellationToken::new())
        .await
        .expect_err("allowlist must restrict guest paths");
    assert!(matches!(error, EnvError::PermissionDenied(_)));
    let inside = std::path::PathBuf::from("/workspace/allowed/file.txt");
    env.write_file(&inside, b"allowed\n", CancellationToken::new())
        .await
        .expect("write allowed path");
    if bwrap_available() {
        let error = match sandbox
            .env()
            .execute_shell(
                "touch /workspace/outside.txt",
                shell_options(Duration::from_secs(5)),
            )
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("shell allowlist must restrict workspace root"),
        };
        assert!(
            error.to_string().contains("shell_failed"),
            "unexpected shell allowlist error: {error:?}"
        );
        sandbox
            .env()
            .execute_shell(
                "printf shell > /workspace/allowed/shell.txt",
                shell_options(Duration::from_secs(5)),
            )
            .await
            .expect("write allowed path from shell");
        let text = sandbox
            .env()
            .read_text_file(
                &std::path::PathBuf::from("/workspace/allowed/shell.txt"),
                CancellationToken::new(),
            )
            .await
            .expect("read shell-created file");
        assert_eq!(text, "shell");
    }
    Box::new(sandbox)
        .shutdown()
        .await
        .expect("shutdown sandbox");
}

#[tokio::test]
async fn gateway_principal_allowlist_uses_path_components() {
    let mut restricted = principal("tenant-a", TOKEN_A);
    restricted.fs_allowlist = vec![std::path::PathBuf::from("/workspace/allow")];
    let gateway = start_gateway(vec![restricted]).await;
    let client = remote_client(&gateway.base_url, TOKEN_A);
    let mut config = sandbox_config();
    config.fs_allowlist = vec![std::path::PathBuf::from("/workspace/allowed")];
    let error = match RemoteSandbox::connect(config, client).await {
        Err(error) => error,
        Ok(_) => panic!("sibling prefix must not be authorized"),
    };
    assert!(matches!(error, RemoteSandboxError::Gateway { .. }));
}

#[tokio::test]
async fn gateway_invalid_guest_allowlist_does_not_leave_a_work_root() {
    let gateway = start_gateway(vec![principal("tenant-a", TOKEN_A)]).await;
    let mut config = sandbox_config();
    config.fs_allowlist = vec![std::path::PathBuf::from("/etc")];
    let error =
        match RemoteSandbox::connect(config, remote_client(&gateway.base_url, TOKEN_A)).await {
            Ok(_) => panic!("gateway must reject paths outside /workspace"),
            Err(error) => error,
        };
    assert!(
        error
            .to_string()
            .contains("fs_allowlist entries must be /workspace or a subpath")
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
    assert_eq!(work_dir, std::path::Path::new("/workspace"));

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
    let output = sandbox
        .env()
        .execute_shell("pwd", shell_options(Duration::from_secs(5)))
        .await
        .expect("execute pwd");
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, "/workspace\n");

    let temp_dir = sandbox
        .env()
        .create_temp_dir("agent")
        .await
        .expect("temp dir");
    assert!(temp_dir.starts_with(work_dir.join(".tmp")));
    assert!(
        sandbox
            .env()
            .exists(&temp_dir, CancellationToken::new())
            .await
            .expect("temp dir exists")
    );
    sandbox.env().cleanup().await.expect("cleanup temp dirs");
    assert!(
        !sandbox
            .env()
            .exists(&work_dir.join(".tmp"), CancellationToken::new())
            .await
            .expect("temp root exists")
    );

    let outside = std::path::PathBuf::from("/etc/llm-harness-gateway-test");
    let error = sandbox
        .env()
        .write_file(&outside, b"forbidden", CancellationToken::new())
        .await
        .expect_err("filesystem boundary must be enforced");
    assert!(matches!(error, EnvError::PermissionDenied(_)));

    sandbox.reset().await.expect("reset sandbox");
    assert!(
        !sandbox
            .env()
            .exists(&file, CancellationToken::new())
            .await
            .expect("file exists after reset")
    );

    Box::new(sandbox)
        .shutdown()
        .await
        .expect("shutdown sandbox");
    assert_eq!(
        std::fs::read_dir(gateway.work_root().join("sandboxes"))
            .expect("read sandbox root")
            .count(),
        0
    );
}

#[tokio::test]
async fn gateway_uploads_a_batch_of_text_and_binary_files() {
    if !bwrap_available() {
        return;
    }
    let gateway = start_gateway(vec![principal("tenant-a", TOKEN_A)]).await;
    let client = remote_client(&gateway.base_url, TOKEN_A);
    let sandbox = client
        .create_sandbox(sandbox_config())
        .await
        .expect("create sandbox");

    let text_path = PathBuf::from(&sandbox.work_dir).join("notes.txt");
    let binary_path = PathBuf::from(&sandbox.work_dir).join("payload.bin");
    let uploaded = client
        .upload_files(
            &sandbox.sandbox_id,
            &[
                (text_path.as_path(), b"text file\n".as_slice()),
                (binary_path.as_path(), [0_u8, 1, 255].as_slice()),
            ],
        )
        .await
        .expect("upload files");

    assert_eq!(
        uploaded,
        [
            llm_harness_sandbox_remote::protocol::UploadedFile {
                path: text_path.to_string_lossy().into_owned(),
                bytes: 10,
            },
            llm_harness_sandbox_remote::protocol::UploadedFile {
                path: binary_path.to_string_lossy().into_owned(),
                bytes: 3,
            },
        ]
    );
    assert_eq!(
        client
            .read_file(&sandbox.sandbox_id, &text_path, false)
            .await
            .expect("read uploaded text"),
        b"text file\n"
    );
    assert_eq!(
        client
            .read_file(&sandbox.sandbox_id, &binary_path, true)
            .await
            .expect("read uploaded binary"),
        [0_u8, 1, 255]
    );

    let duplicate = UploadFilesRequest {
        files: vec![
            UploadFile {
                path: text_path.to_string_lossy().into_owned(),
                content: FileContent::Text {
                    value: "first".to_owned(),
                },
            },
            UploadFile {
                path: text_path.to_string_lossy().into_owned(),
                content: FileContent::Text {
                    value: "second".to_owned(),
                },
            },
        ],
    };
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/sandboxes/{}/fs/upload",
            gateway.base_url, sandbox.sandbox_id
        ))
        .header("Authorization", format!("Bearer {TOKEN_A}"))
        .json(&duplicate)
        .send()
        .await
        .expect("duplicate upload request");
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);

    client
        .delete_sandbox(&sandbox.sandbox_id)
        .await
        .expect("delete sandbox");
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
    assert!(
        client
            .exists(&response.sandbox_id, &file)
            .await
            .expect("file exists before restart")
    );

    let work_root = first.stop().await;
    assert!(work_root_contains_file(&work_root, "restart.txt"));

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
    assert!(!work_root_contains_file(&work_root, "restart.txt"));
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

#[tokio::test]
#[cfg(target_os = "linux")]
async fn firecracker_gateway_restores_restarted_sandboxes_as_creating() {
    let artifacts = firecracker_artifact_root();
    let work_root = private_tempdir();
    let config = GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
        .expect("gateway config")
        .with_work_root(work_root.path().to_path_buf())
        .with_backend(firecracker_backend(artifacts.path(), 3));
    drop(SandboxGateway::new(config.clone()).expect("initialize registry"));
    let sandbox_id = uuid::Uuid::new_v4().to_string();
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(work_root.path().join("sandboxes").join(&sandbox_id))
        .expect("create Firecracker work root");
    insert_test_registry_record(
        work_root.path(),
        &sandbox_id,
        "tenant-a",
        &persisted_sandbox_config_json(),
        "running",
    );
    let connection =
        Connection::open(work_root.path().join("registry.sqlite")).expect("open registry");
    connection
        .execute(
            "UPDATE sandboxes SET vsock_guest_cid = 3 WHERE sandbox_id = ?1",
            [&sandbox_id],
        )
        .expect("persist Firecracker CID");

    let gateway = SandboxGateway::new(config).expect("restore Firecracker sandbox");
    assert_eq!(registry_record_count(work_root.path()), 1);
    let connection =
        Connection::open(work_root.path().join("registry.sqlite")).expect("open registry");
    let (state, cid): (String, i64) = connection
        .query_row(
            "SELECT state, vsock_guest_cid FROM sandboxes WHERE sandbox_id = ?1",
            [&sandbox_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read restored record");
    assert_eq!(state, "creating");
    assert_eq!(cid, 3);
    gateway
        .shutdown_all()
        .await
        .expect("shut down restored gateway");
    assert_eq!(registry_record_count(work_root.path()), 0);
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn firecracker_gateway_starts_a_recovered_sandbox_only_after_guest_readiness() {
    let artifacts = firecracker_artifact_root();
    write_fake_firecracker(artifacts.path().join("firecracker").as_path(), 6000);
    let work_root = private_tempdir();
    let config = GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
        .expect("gateway config")
        .with_work_root(work_root.path().to_path_buf())
        .with_backend(firecracker_backend(artifacts.path(), 3));
    drop(SandboxGateway::new(config.clone()).expect("initialize registry"));
    let sandbox_id = uuid::Uuid::new_v4().to_string();
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(work_root.path().join("sandboxes").join(&sandbox_id))
        .expect("create Firecracker work root");
    insert_test_registry_record(
        work_root.path(),
        &sandbox_id,
        "tenant-a",
        &persisted_sandbox_config_json(),
        "creating",
    );
    Connection::open(work_root.path().join("registry.sqlite"))
        .expect("open registry")
        .execute(
            "UPDATE sandboxes SET vsock_guest_cid = 3 WHERE sandbox_id = ?1",
            [&sandbox_id],
        )
        .expect("persist Firecracker CID");

    let work_root_path = work_root.path().to_path_buf();
    let mut test_gateway = start_gateway_at(config, work_root_path).await;
    test_gateway._work_root = Some(work_root);
    let client = remote_client(&test_gateway.base_url, TOKEN_A);
    let status = client
        .get_sandbox(&sandbox_id)
        .await
        .expect("read restored sandbox");
    assert_eq!(status.state, RemoteSandboxState::Creating);

    client
        .start_sandbox(&sandbox_id)
        .await
        .expect("start recovered Firecracker sandbox");
    let status = client
        .get_sandbox(&sandbox_id)
        .await
        .expect("read started sandbox");
    assert_eq!(status.state, RemoteSandboxState::Running);
    let connection =
        Connection::open(test_gateway.work_root().join("registry.sqlite")).expect("open registry");
    let (state, cid): (String, i64) = connection
        .query_row(
            "SELECT state, vsock_guest_cid FROM sandboxes WHERE sandbox_id = ?1",
            [&sandbox_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read started record");
    assert_eq!(state, "running");
    assert_eq!(cid, 3);
    test_gateway
        ._gateway
        .shutdown_all()
        .await
        .expect("shut down recovered gateway");
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn firecracker_gateway_keeps_a_recovered_sandbox_creating_after_start_failure() {
    let artifacts = firecracker_artifact_root();
    let work_root = private_tempdir();
    let config = GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
        .expect("gateway config")
        .with_work_root(work_root.path().to_path_buf())
        .with_backend(firecracker_backend(artifacts.path(), 3));
    drop(SandboxGateway::new(config.clone()).expect("initialize registry"));
    let sandbox_id = uuid::Uuid::new_v4().to_string();
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(work_root.path().join("sandboxes").join(&sandbox_id))
        .expect("create Firecracker work root");
    insert_test_registry_record(
        work_root.path(),
        &sandbox_id,
        "tenant-a",
        &persisted_sandbox_config_json(),
        "creating",
    );
    Connection::open(work_root.path().join("registry.sqlite"))
        .expect("open registry")
        .execute(
            "UPDATE sandboxes SET vsock_guest_cid = 3 WHERE sandbox_id = ?1",
            [&sandbox_id],
        )
        .expect("persist Firecracker CID");

    let work_root_path = work_root.path().to_path_buf();
    let mut test_gateway = start_gateway_at(config, work_root_path).await;
    test_gateway._work_root = Some(work_root);
    let client = remote_client(&test_gateway.base_url, TOKEN_A);
    let error = client
        .start_sandbox(&sandbox_id)
        .await
        .expect_err("invalid Firecracker binary must fail startup");
    match error {
        RemoteSandboxError::Gateway { status, .. } => assert_eq!(status, 503),
        error => panic!("expected gateway error, got {error:?}"),
    }
    let status = client
        .get_sandbox(&sandbox_id)
        .await
        .expect("read failed recovered sandbox");
    assert_eq!(status.state, RemoteSandboxState::Creating);
    let connection =
        Connection::open(test_gateway.work_root().join("registry.sqlite")).expect("open registry");
    let state: String = connection
        .query_row(
            "SELECT state FROM sandboxes WHERE sandbox_id = ?1",
            [&sandbox_id],
            |row| row.get(0),
        )
        .expect("read failed record");
    assert_eq!(state, "creating");
    assert_eq!(registry_record_count(test_gateway.work_root()), 1);
    test_gateway
        ._gateway
        .shutdown_all()
        .await
        .expect("shut down failed recovery gateway");
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

/// Capacity rejections must be the gateway's HTTP 503 with the dedicated
/// `sandbox_limit_reached` code — never a transport error, an invalid config,
/// or a different server failure that merely happens to co-occur.
fn assert_capacity_rejection(error: &RemoteSandboxError) {
    match error {
        RemoteSandboxError::Gateway {
            status,
            code,
            message,
            ..
        } => {
            assert_eq!(
                *status,
                reqwest::StatusCode::SERVICE_UNAVAILABLE.as_u16(),
                "capacity rejection must be HTTP 503: {message}"
            );
            assert_eq!(
                code, "sandbox_limit_reached",
                "capacity rejection must use the sandbox_limit_reached code"
            );
        }
        other => panic!("expected an HTTP 503 sandbox_limit_reached rejection, got {other:?}"),
    }
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
    let work_root = gateway.work_root().to_path_buf();
    let client = remote_client(&gateway.base_url, TOKEN_A);

    // `tokio::join!` does not order the two requests as they arrive at the
    // gateway, so either arm may win the single slot. The invariant is that
    // exactly one creation is admitted.
    let (first, second) = tokio::join!(
        client.create_sandbox(sandbox_config()),
        client.create_sandbox(sandbox_config())
    );
    let (created, rejected) = match (first, second) {
        (Ok(created), Err(rejected)) | (Err(rejected), Ok(created)) => (created, rejected),
        (Ok(first), Ok(second)) => panic!(
            "max_sandboxes=1 admitted two concurrent creations: {} and {}",
            first.sandbox_id, second.sandbox_id
        ),
        (Err(first), Err(second)) => {
            panic!("max_sandboxes=1 rejected both concurrent creations: {first}; {second}")
        }
    };
    assert_capacity_rejection(&rejected);
    assert_eq!(
        registry_record_count(&work_root),
        1,
        "only the admitted sandbox may be registered"
    );

    // With the only slot occupied, a third creation must be rejected the same
    // way and must not register anything.
    let third = client
        .create_sandbox(sandbox_config())
        .await
        .expect_err("third creation must be rejected while the slot is occupied");
    assert_capacity_rejection(&third);
    assert_eq!(
        registry_record_count(&work_root),
        1,
        "a rejected creation must not add a registry record"
    );

    // Deleting the admitted sandbox releases the slot.
    client
        .delete_sandbox(&created.sandbox_id)
        .await
        .expect("delete sandbox");
    assert_eq!(
        registry_record_count(&work_root),
        0,
        "deleting the admitted sandbox must remove its registry record"
    );
    let replacement = client
        .create_sandbox(sandbox_config())
        .await
        .expect("capacity must recover after the admitted sandbox is deleted");
    client
        .delete_sandbox(&replacement.sandbox_id)
        .await
        .expect("delete replacement sandbox");
    assert_eq!(
        registry_record_count(&work_root),
        0,
        "replacement deletion must leave no registry records"
    );
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

#[cfg(target_os = "macos")]
#[tokio::test]
async fn create_response_echoes_host_binds() {
    let project = private_tempdir();
    let mut trusted = principal("tenant-a", TOKEN_A);
    trusted.host_project_dirs = vec![project.path().to_path_buf()];
    trusted.max_trust_profile = llm_harness_platform::trust::TrustProfile::TrustedFull;
    let gateway = start_gateway(vec![trusted]).await;
    let client = remote_client(&gateway.base_url, TOKEN_A);
    let mut request = sandbox_config();
    request.trust_profile = llm_harness_platform::trust::TrustProfile::TrustedFull;
    let response = client.create_sandbox(request).await.expect("create");
    assert_eq!(response.host_binds.len(), 1, "response must echo binds");
    assert_eq!(
        std::path::Path::new(&response.host_binds[0].host),
        project.path()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn gateway_rejects_os_backend_without_creating_a_work_root() {
    let parent = tempfile::tempdir().expect("parent directory");
    let work_root = parent.path().join("unsupported-os");
    let config = GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
        .expect("gateway config")
        .with_work_root(work_root.clone())
        .with_backend(GatewayBackend::Os);
    let error = match SandboxGateway::new(config) {
        Ok(_) => panic!("Linux must not fall back to host execution"),
        Err(error) => error,
    };
    assert_eq!(error.to_string(), "os backend is only available on macOS");
    assert!(!work_root.exists());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn bwrap_create_response_only_echoes_configured_binds() {
    let project = private_tempdir();
    let mut tenant = principal("tenant-a", TOKEN_A);
    tenant.host_project_dirs = vec![project.path().to_path_buf()];
    let gateway = start_gateway(vec![tenant]).await;
    let client = remote_client(&gateway.base_url, TOKEN_A);
    let response = client
        .create_sandbox(sandbox_config())
        .await
        .expect("create");
    assert!(response.host_binds.is_empty());
}

/// Refuse to run (rather than silently skipping) unless this kernel can
/// actually create an unprivileged bwrap namespace. `bwrap --version` only
/// proves the binary exists, so execute a real sandboxed process.
#[cfg(target_os = "linux")]
fn require_bwrap_namespace_isolation() {
    if !bwrap_available() {
        panic!("BLOCKED: bwrap is not installed, so real sandbox isolation cannot be verified");
    }
    let output = std::process::Command::new("bwrap")
        .args([
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--ro-bind",
            "/",
            "/",
            "--",
            "/bin/true",
        ])
        .output()
        .expect("run bwrap namespace probe");
    assert!(
        output.status.success(),
        "BLOCKED: bwrap cannot create an unprivileged namespace on this kernel: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
}

#[cfg(target_os = "linux")]
fn assert_gateway_error(
    error: &RemoteSandboxError,
    expected_status: reqwest::StatusCode,
    expected_code: &str,
    context: &str,
) {
    match error {
        RemoteSandboxError::Gateway {
            status,
            code,
            message,
            ..
        } => {
            assert_eq!(*status, expected_status.as_u16(), "{context}: {message}");
            assert_eq!(code, expected_code, "{context}: {message}");
        }
        other => panic!("{context}: expected an HTTP gateway error, got {other:?}"),
    }
}

/// End-to-end negative isolation check for the Linux bwrap backend.
///
/// A trusted-profile principal is configured with a host project dir and the
/// sandbox is created with `TrustedFull`, which must not accidentally expose
/// that (unauthorized) host path. The sentinel must be unreadable both through
/// the gateway file API and from inside the real bwrap namespace, while the
/// sandbox's own work dir stays readable.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn bwrap_trusted_profile_cannot_reach_unauthorized_host_paths() {
    require_bwrap_namespace_isolation();

    let host_secret = private_tempdir();
    let sentinel = host_secret.path().join("sentinel.txt");
    let secret_contents = "host-only-sentinel-7f3c1a";
    std::fs::write(&sentinel, secret_contents).expect("write host sentinel");
    // The sentinel must genuinely exist and be readable on the host; otherwise
    // the negative assertions below would pass vacuously.
    assert_eq!(
        std::fs::read_to_string(&sentinel).expect("read host sentinel"),
        secret_contents
    );

    let mut trusted = principal("tenant-a", TOKEN_A);
    trusted.host_project_dirs = vec![host_secret.path().to_path_buf()];
    trusted.max_trust_profile = llm_harness_platform::trust::TrustProfile::TrustedFull;
    let gateway = start_gateway(vec![trusted]).await;
    let client = remote_client(&gateway.base_url, TOKEN_A);

    let mut request = sandbox_config();
    request.trust_profile = llm_harness_platform::trust::TrustProfile::TrustedFull;
    let sandbox = RemoteSandbox::connect(request, Arc::clone(&client))
        .await
        .expect("trusted-profile sandbox creation");
    assert!(
        sandbox.host_binds().is_empty(),
        "bwrap must not bind principal host project dirs: {:?}",
        sandbox.host_binds()
    );
    let sandbox_id = sandbox.sandbox_id().to_owned();
    let env = sandbox.env();

    // 1. The gateway file API never accepts raw host paths.
    let error = client
        .read_file(&sandbox_id, &sentinel, false)
        .await
        .expect_err("file API must reject an absolute host path");
    assert_gateway_error(
        &error,
        reqwest::StatusCode::FORBIDDEN,
        "path_outside_workspace",
        "raw host path through the file API",
    );

    // 2. The host path is genuinely invisible from inside the bwrap namespace.
    //    A failed command is surfaced as an error rather than an exit status.
    let command = format!("cat -- {}", sentinel.to_string_lossy());
    let error = match env
        .execute_shell(&command, shell_options(Duration::from_secs(5)))
        .await
    {
        Ok(output) => panic!(
            "shell must not read the host sentinel: exit={} stdout={} stderr={}",
            output.exit_code, output.stdout, output.stderr
        ),
        Err(error) => error,
    };
    assert!(
        !matches!(
            error,
            EnvError::ShellTimeout { .. } | EnvError::ShellAborted { .. }
        ),
        "host read must fail as a denied command, not a timeout/abort: {error}"
    );
    assert!(
        !error.to_string().contains(secret_contents),
        "host sentinel leaked through the shell error: {error}"
    );
    let visible = env
        .execute_shell(
            &format!(
                "test -e {} && echo VISIBLE || echo INVISIBLE",
                sentinel.to_string_lossy()
            ),
            shell_options(Duration::from_secs(5)),
        )
        .await
        .expect("execute shell");
    assert!(
        visible.stdout.contains("INVISIBLE"),
        "host path must not exist inside the namespace: exit={} stdout={} stderr={}",
        visible.exit_code,
        visible.stdout,
        visible.stderr
    );

    // 3. Even a guest-visible symlink to the host path is blocked by cap-std:
    //    `/workspace/host-link` passes gateway path validation, so the denial
    //    must come from the sandbox filesystem layer.
    let link = "/workspace/host-link";
    let linked = env
        .execute_shell(
            &format!("ln -s {} {link} && echo LINKED", sentinel.to_string_lossy()),
            shell_options(Duration::from_secs(5)),
        )
        .await
        .expect("execute shell");
    assert!(
        linked.stdout.contains("LINKED"),
        "failed to create the escape symlink: exit={} stdout={} stderr={}",
        linked.exit_code,
        linked.stdout,
        linked.stderr
    );
    let error = client
        .read_file(&sandbox_id, Path::new(link), false)
        .await
        .expect_err("file API must block a symlink escaping the work dir");
    assert_gateway_error(
        &error,
        reqwest::StatusCode::FORBIDDEN,
        "forbidden",
        "symlink escape through the file API",
    );

    // 4. Positive control: the same shell and file API do work for the
    //    sandbox's own work dir, so the denials above are not a broken sandbox.
    let own_file = Path::new("/workspace/visible.txt");
    client
        .write_file(&sandbox_id, own_file, b"sandbox-visible", false)
        .await
        .expect("write sandbox file");
    let output = env
        .execute_shell(
            "cat -- /workspace/visible.txt",
            shell_options(Duration::from_secs(5)),
        )
        .await
        .expect("execute shell");
    assert_eq!(output.exit_code, 0, "sandbox work dir must be readable");
    assert_eq!(output.stdout, "sandbox-visible");

    // The sentinel stays intact and host-only throughout.
    assert_eq!(
        std::fs::read_to_string(&sentinel).expect("read host sentinel"),
        secret_contents
    );

    client
        .delete_sandbox(&sandbox_id)
        .await
        .expect("delete sandbox");
}
