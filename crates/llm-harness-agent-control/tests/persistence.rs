use llm_harness_agent_control::SessionPersistence;

#[test]
fn persistence_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.json");
    let persistence = SessionPersistence::new(&path);

    let loaded = persistence.load().unwrap();
    assert!(loaded.sandbox_ids.is_empty());

    persistence.add_sandbox("sbx-1").unwrap();
    persistence.add_sandbox("sbx-2").unwrap();
    let loaded = persistence.load().unwrap();
    assert_eq!(loaded.sandbox_ids, vec!["sbx-1", "sbx-2"]);

    persistence.remove_sandbox("sbx-1").unwrap();
    let loaded = persistence.load().unwrap();
    assert_eq!(loaded.sandbox_ids, vec!["sbx-2"]);

    persistence.clear().unwrap();
    let loaded = persistence.load().unwrap();
    assert!(loaded.sandbox_ids.is_empty());
}

#[test]
fn persistence_no_duplicate_add() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.json");
    let persistence = SessionPersistence::new(&path);

    persistence.add_sandbox("sbx-1").unwrap();
    persistence.add_sandbox("sbx-1").unwrap();
    let loaded = persistence.load().unwrap();
    assert_eq!(loaded.sandbox_ids.len(), 1);
}

#[test]
fn persistence_load_missing_file_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nonexistent.json");
    let persistence = SessionPersistence::new(&path);
    let loaded = persistence.load().unwrap();
    assert!(loaded.sandbox_ids.is_empty());
}
