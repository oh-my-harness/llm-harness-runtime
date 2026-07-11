use super::*;

// ── Branch ops: Branching phase guard ────────────────────────────────────

#[tokio::test]
async fn branch_ops_reject_during_turning() {
    let h = make_harness(vec![]);
    {
        let mut inner = h.inner.lock().unwrap();
        inner.state.phase = HarnessPhase::Turning;
    }

    // fork_branch
    let err = h.fork_branch(EntryId::new(), None).await;
    assert!(matches!(
        err,
        Err(HarnessError::NotIdle(HarnessPhase::Turning))
    ));

    // navigate_tree
    let err = h.navigate_tree(EntryId::new()).await;
    assert!(matches!(
        err,
        Err(HarnessError::NotIdle(HarnessPhase::Turning))
    ));

    // delete_branch
    let err = h.delete_branch(EntryId::new()).await;
    assert!(matches!(
        err,
        Err(HarnessError::NotIdle(HarnessPhase::Turning))
    ));

    // generate_branch_summary
    let err = h.generate_branch_summary(EntryId::new()).await;
    assert!(matches!(
        err,
        Err(HarnessError::NotIdle(HarnessPhase::Turning))
    ));

    h.inner.lock().unwrap().state.phase = HarnessPhase::Idle;
}

#[tokio::test]
async fn branch_ops_restore_idle_after_completion() {
    let h = make_harness(vec![]);

    // fork_branch: failure path (empty session → entry not found) still
    // must restore Idle via PhaseGuard::drop.
    let entries = h.read_active_path().await.unwrap();
    let from = entries.last().map(|e| e.id).unwrap_or(EntryId::new());
    let _ = h.fork_branch(from, Some("test".into())).await;
    assert_eq!(h.state().phase, HarnessPhase::Idle);

    // navigate_tree: same target id; phase must return to Idle.
    let _ = h.navigate_tree(from).await;
    assert_eq!(h.state().phase, HarnessPhase::Idle);
}
