//! Tests for the session manager's local state sidecar and mapping logic.
//!
//! Gateway/LLM integration is covered elsewhere; these tests pin the pure
//! logic: sidecar roundtrip, defaults, and summary ordering inputs.

use llm_harness_mobile_core::manager::state::{SessionState, read_state, write_state};

#[test]
fn state_sidecar_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s1");
    std::fs::create_dir_all(&path).unwrap();
    let state = SessionState {
        sandbox_id: "abc".to_owned(),
        trust_profile: "trusted-lan".to_owned(),
        last_active_at_ms: 42,
    };
    write_state(&path, &state).unwrap();
    let loaded = read_state(&path).unwrap();
    assert_eq!(loaded.sandbox_id, "abc");
    assert_eq!(loaded.trust_profile, "trusted-lan");
    assert_eq!(loaded.last_active_at_ms, 42);
}

#[test]
fn read_state_missing_file_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s2");
    std::fs::create_dir_all(&path).unwrap();
    let loaded = read_state(&path).unwrap();
    assert!(loaded.sandbox_id.is_empty());
    assert!(loaded.trust_profile.is_empty());
    assert_eq!(loaded.last_active_at_ms, 0);
}

#[test]
fn read_state_corrupt_file_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s3");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("state.json"), b"not json").unwrap();
    let loaded = read_state(&path).unwrap();
    assert!(loaded.sandbox_id.is_empty());
}

#[test]
fn state_sidecar_updates_last_active() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s4");
    std::fs::create_dir_all(&path).unwrap();
    let mut state = SessionState::default();
    state.touch(1000);
    write_state(&path, &state).unwrap();
    let loaded = read_state(&path).unwrap();
    assert_eq!(loaded.last_active_at_ms, 1000);
}
