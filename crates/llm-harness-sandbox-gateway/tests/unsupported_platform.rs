#![cfg(not(unix))]

use llm_harness_sandbox_gateway::{
    GatewayBackend, GatewayConfig, GatewayPrincipalConfig, SandboxGateway,
};

#[test]
fn unsupported_platform_rejects_execution_before_creating_storage() {
    let parent = tempfile::tempdir().expect("parent directory");
    for (backend, expected) in [
        (
            GatewayBackend::Bwrap,
            "gateway work root requires Unix permission enforcement",
        ),
        (GatewayBackend::Os, "os backend is only available on macOS"),
    ] {
        let work_root = parent.path().join("unavailable-gateway");
        let config = GatewayConfig::new(vec![GatewayPrincipalConfig {
            principal: "tenant".into(),
            bearer_token: "a".repeat(32),
            fs_allowlist: vec!["/workspace".into()],
            host_project_dirs: vec![],
            max_trust_profile: Default::default(),
            requests_per_minute: None,
            max_active_sandboxes: None,
        }])
        .expect("valid principal")
        .with_work_root(work_root.clone())
        .with_backend(backend);
        let error = match SandboxGateway::new(config) {
            Ok(_) => panic!("unsupported gateway must not execute on the host"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), expected);
        assert!(!work_root.exists());
    }
}
