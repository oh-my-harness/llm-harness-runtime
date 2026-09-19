use llm_harness_runtime_app_shell::core_process::{
    CoreProcess, CoreProcessConfig, CoreProcessError, parse_panel_descriptor_line,
    read_panel_descriptor,
};
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;

#[test]
fn parses_panel_descriptor_line() {
    let line = "RUNTIME_APP_PANEL_DESCRIPTOR=/tmp/runtime-app/panel.json\r\n";
    assert_eq!(
        parse_panel_descriptor_line(line),
        Some(PathBuf::from("/tmp/runtime-app/panel.json"))
    );
    assert_eq!(
        parse_panel_descriptor_line("STUDIO_PANEL_DESCRIPTOR=/tmp/panel.json"),
        None
    );
}

#[test]
fn reads_and_validates_panel_descriptor() {
    let root = TempDir::new().unwrap();
    let descriptor_path = root.path().join("panel.json");
    std::fs::write(
        &descriptor_path,
        r#"{"schema":"llm-harness.studio.panel-descriptor.v1","url":"http://127.0.0.1:12345/app/agent-team?token=secret"}"#,
    )
    .unwrap();

    let panel_url = read_panel_descriptor(&descriptor_path).unwrap();
    assert_eq!(panel_url.scheme(), "http");
    assert_eq!(panel_url.host_str(), Some("127.0.0.1"));
    assert_eq!(panel_url.port(), Some(12345));
    assert_eq!(panel_url.path(), "/app/agent-team");

    std::fs::write(
        &descriptor_path,
        r#"{"schema":"llm-harness.studio.panel-descriptor.v1","url":"https://example.com/app"}"#,
    )
    .unwrap();
    assert!(matches!(
        read_panel_descriptor(&descriptor_path),
        Err(CoreProcessError::NonLoopbackPanelUrl(_))
    ));

    std::fs::write(
        &descriptor_path,
        r#"{"schema":"unexpected","url":"http://127.0.0.1:12345/app"}"#,
    )
    .unwrap();
    assert!(matches!(
        read_panel_descriptor(&descriptor_path),
        Err(CoreProcessError::UnexpectedDescriptorSchema(_))
    ));
}

#[test]
fn core_process_starts_reports_panel_and_shuts_down() {
    let root = TempDir::new().unwrap();
    let config = CoreProcessConfig {
        binary_path: env!("CARGO_BIN_EXE_llm-harness-app").into(),
        data_root: Some(root.path().to_path_buf()),
        startup_timeout: Duration::from_secs(30),
        shutdown_timeout: Duration::from_secs(10),
    };

    let core_process = CoreProcess::spawn(config).unwrap();
    let panel_url = core_process.panel_url();
    assert_eq!(panel_url.scheme(), "http");
    assert!(matches!(
        panel_url.host_str(),
        Some("127.0.0.1") | Some("localhost")
    ));
    assert_eq!(panel_url.path(), "/app/agent-team");

    match core_process.shutdown() {
        llm_harness_runtime_app_shell::core_process::ShutdownOutcome::Graceful(status) => {
            assert!(status.success(), "llm-harness-app exited with {status}");
        }
        outcome => panic!("expected graceful shutdown, got {outcome:?}"),
    }
    assert!(!root.path().join("panel.json").exists());
}
