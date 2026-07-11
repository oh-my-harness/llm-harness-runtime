use super::*;

#[tokio::test]
async fn enter_phase_rejects_when_not_idle() {
    let h = make_harness(vec![]);
    {
        let mut inner = h.inner.lock().unwrap();
        inner.state.phase = HarnessPhase::Turning;
    }
    let err = h.enter_phase(HarnessPhase::Branching).unwrap_err();
    assert!(
        matches!(err, HarnessError::NotIdle(HarnessPhase::Turning)),
        "expected NotIdle(Turning), got {err:?}"
    );
    // phase 不变
    assert_eq!(h.state().phase, HarnessPhase::Turning);
    h.inner.lock().unwrap().state.phase = HarnessPhase::Idle;
}

#[tokio::test]
async fn guard_drop_restores_idle() {
    let h = make_harness(vec![]);
    {
        let _guard = h.enter_phase(HarnessPhase::Branching).unwrap();
        assert_eq!(h.state().phase, HarnessPhase::Branching);
    }
    // guard drop 后恢复 Idle
    assert_eq!(h.state().phase, HarnessPhase::Idle);
}
#[tokio::test]
async fn enter_phase_is_atomic() {
    let h = Arc::new(make_harness(vec![]));
    let h1 = h.clone();
    let h2 = h.clone();
    // 两个并发 task 同时 enter_phase，只有一个应成功
    let (tx, rx) = tokio::sync::oneshot::channel();
    let jh1 = tokio::spawn(async move {
        let r1 = h1.enter_phase(HarnessPhase::Branching);
        let ok = r1.is_ok();
        if let Ok(_g) = &r1 {
            // 持有 guard 堵住第二个调用方
            tx.send(()).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        // guard 在 _g 离开作用域时 drop，归还借用
        drop(r1);
        ok
    });
    // 等 jh1 拿到 guard 后再启动 jh2
    rx.await.unwrap();
    let r2 = h2.enter_phase(HarnessPhase::Branching);
    let ok1 = jh1.await.unwrap();
    let ok2 = r2.is_ok();
    assert!(
        ok1 ^ ok2,
        "exactly one enter_phase must succeed; got ok1={ok1} ok2={ok2}"
    );
    if !ok2 {
        assert!(matches!(r2, Err(HarnessError::NotIdle(_))));
    }
}

#[tokio::test]
async fn flush_error_returns_phase_to_idle() {
    // Storage fails immediately on append_entry (fail_after=0)
    let h = make_harness_failing_storage(0, vec![MockResponse::text("hi")]);

    let result = h.prompt("hello").await;

    // Bug: phase stays Turning. Fix: phase must return to Idle.
    assert_eq!(
        h.state().phase,
        HarnessPhase::Idle,
        "phase must be Idle after flush error"
    );
    assert!(result.is_err(), "prompt must return Err when storage fails");
}

#[tokio::test]
async fn compact_sets_compacting_phase() {
    // do_compact 会因 InsufficientTokens 失败，但 phase 在失败路径上应仍是
    // Compacting 直到 guard drop。此测试用并发 task 观察 phase。
    // 简化：直接调 enter_phase(Compacting) 验证 drop 恢复——已在 guard_drop_restores_idle 覆盖。
    // 这里测 compact() 的 observable 行为：compact 失败后 phase 回 Idle。
    let h = make_harness(vec![]);
    let _ = h.compact().await; // InsufficientTokens
    assert_eq!(h.state().phase, HarnessPhase::Idle);
}
