use std::sync::Arc;
/// Monotonic clock for latency measurement: `std::time::Instant` on native,
/// `web_time::Instant` on wasm32 (std's panics there — no clock source in
/// wasm32-unknown-unknown).
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use futures::future::BoxFuture;
use llm_harness_types::*;

/// Decorates a `Tool` with optional before/after hooks. Stateless.
///
/// A before-hook `Deny` rejects the call without executing the inner tool,
/// but the failure still flows through the after-hook (`result: Err`,
/// near-zero duration) so observers such as audit/tracing record the denied
/// attempt instead of it vanishing without a trace.
pub struct HookedTool {
    /// The inner tool implementation.
    pub inner: Arc<dyn Tool>,
    /// Optional pre-execution hook.
    pub before: Option<Arc<dyn BeforeToolCallHook>>,
    /// Optional post-execution hook.
    pub after: Option<Arc<dyn AfterToolCallHook>>,
}

impl Tool for HookedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn level(&self) -> ToolLevel {
        self.inner.level()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters_schema(&self) -> &serde_json::Value {
        self.inner.parameters_schema()
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        self.inner.execution_mode()
    }
    fn report_duration(&self) -> bool {
        self.inner.report_duration()
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            // Apply before-hook. `denied` carries the Deny failure: the call
            // is rejected, but still flows through the after-hook below so
            // observers (audit, tracing) see the denied attempt instead of the
            // call vanishing without a trace.
            let (effective_args, denied) = if let Some(h) = &self.before {
                let before_ctx = BeforeToolCallCtx {
                    run: &ctx.run,
                    assistant_message: &ctx.assistant_message,
                    tool_use_id: &ctx.tool_use_id,
                    tool_name: self.inner.name(),
                    args: &args,
                    turn_index: ctx.turn_index,
                };
                match h.on_call(before_ctx).await {
                    BeforeToolCallDecision::Allow => (args, None),
                    BeforeToolCallDecision::Modify(new_args) => (new_args, None),
                    BeforeToolCallDecision::Deny(failure) => (args, Some(failure)),
                }
            } else {
                (args, None)
            };

            let started = Instant::now();
            let result = match denied {
                // Denied before execution: duration is ~zero; the call still
                // flows through the after-hook below so audit/tracing observe
                // it (fixes denied calls being invisible + start_times leak).
                Some(failure) => Err(failure),
                None => self.inner.execute(effective_args.clone(), ctx).await,
            };
            let duration = started.elapsed();

            // Apply after-hook, then opt-in duration annotation. The annotation
            // is applied AFTER user hooks so a Replace cannot wipe it.
            let result = if let Some(h) = &self.after {
                let after_ctx = AfterToolCallCtx {
                    run: &ctx.run,
                    assistant_message: &ctx.assistant_message,
                    tool_use_id: &ctx.tool_use_id,
                    tool_name: self.inner.name(),
                    args: &effective_args,
                    result: &result,
                    duration,
                    turn_index: ctx.turn_index,
                };
                match h.on_complete(after_ctx).await {
                    AfterToolCallDecision::Passthrough => result,
                    AfterToolCallDecision::Replace(replacement) => replacement,
                }
            } else {
                result
            };

            if self.inner.report_duration() {
                append_duration(result, duration)
            } else {
                result
            }
        })
    }
}

/// 将耗时格式化为人类可读文本（`812ms` / `2.35s`）。
fn format_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.2}s", d.as_secs_f64())
    }
}

/// ToolFailure is ~144 bytes; these helpers take it by value so the lint
/// would trip here. Mirrors agent-team's tools and failure_breaker.rs.
#[allow(clippy::result_large_err)]
fn append_duration(
    result: Result<ToolResult, ToolFailure>,
    duration: std::time::Duration,
) -> Result<ToolResult, ToolFailure> {
    let suffix = format!("[duration: {}]", format_duration(duration));
    match result {
        Ok(mut r) => {
            mirror_duration(&mut r.session_projection, &suffix);
            r.model_content.push(DataBlock::text(suffix));
            Ok(r)
        }
        Err(mut f) => {
            if !f.model_message.is_empty() && !f.model_message.ends_with('\n') {
                f.model_message.push('\n');
            }
            f.model_message.push_str(&suffix);
            mirror_duration(&mut f.session_projection, &suffix);
            Err(f)
        }
    }
}

/// Mirror the duration annotation into the persisted projection so session
/// replay/recall shows what the LLM saw. Same convention as source_tag.rs:
/// "so source tags survive persistence and replay". `Full` materializes from
/// `model_content` at persistence time and needs no mirror.
fn mirror_duration(projection: &mut ToolSessionProjection, suffix: &str) {
    match projection {
        ToolSessionProjection::Full { .. } => {}
        ToolSessionProjection::Projected { content, .. } => {
            content.push(DataBlock::text(suffix.to_string()));
        }
        ToolSessionProjection::Ephemeral { summary, .. } => {
            if !summary.is_empty() && !summary.ends_with('\n') {
                summary.push('\n');
            }
            summary.push_str(suffix);
        }
    }
}

#[cfg(test)]
#[cfg(feature = "test-utils")]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    struct SpyTool {
        called: Arc<AtomicBool>,
        terminate: bool,
    }
    impl Tool for SpyTool {
        fn name(&self) -> &str {
            "spy"
        }
        fn description(&self) -> &str {
            "spy tool"
        }
        fn parameters_schema(&self) -> &serde_json::Value {
            static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
            S.get_or_init(|| serde_json::json!({}))
        }
        fn execute<'a>(
            &'a self,
            _: serde_json::Value,
            _: &'a ToolContext,
        ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
            self.called.store(true, Ordering::SeqCst);
            let terminate = self.terminate;
            Box::pin(async move {
                Ok(ToolResult::full(
                    vec![DataBlock::text("spy result")],
                    serde_json::Value::Null,
                    terminate,
                ))
            })
        }
    }

    fn make_ctx() -> ToolContext {
        let (tx, _) = mpsc::channel(1);
        ToolContext {
            run: Arc::new(RunContext::new(RunRequest::default())),
            env: Arc::new(crate::test_utils::NoOpEnv),
            abort: CancellationToken::new(),
            tool_use_id: "c1".into(),
            turn_index: 0,
            assistant_message: Arc::new(crate::test_utils::test_assistant_message(vec![])),
            update_tx: tx,
        }
    }
    /// report_duration=true 的 spy tool。
    struct TimedTool {
        fail: bool,
        sleep_ms: u64,
    }
    impl Tool for TimedTool {
        fn name(&self) -> &str {
            "timed"
        }
        fn description(&self) -> &str {
            "timed tool"
        }
        fn parameters_schema(&self) -> &serde_json::Value {
            static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
            S.get_or_init(|| serde_json::json!({}))
        }
        fn report_duration(&self) -> bool {
            true
        }
        fn execute<'a>(
            &'a self,
            _: serde_json::Value,
            _: &'a ToolContext,
        ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
            let fail = self.fail;
            let sleep_ms = self.sleep_ms;
            Box::pin(async move {
                if sleep_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
                }
                if fail {
                    Err(ToolFailure::new("boom", "timed failure"))
                } else {
                    Ok(ToolResult::full(
                        vec![DataBlock::text("ok")],
                        serde_json::Value::Null,
                        false,
                    ))
                }
            })
        }
    }

    #[tokio::test]
    async fn duration_appended_when_opted_in() {
        let t = HookedTool {
            inner: Arc::new(TimedTool {
                fail: false,
                sleep_ms: 20,
            }),
            before: None,
            after: None,
        };
        let r = t.execute(serde_json::json!({}), &make_ctx()).await.unwrap();
        let last = r.model_content.last().unwrap();
        let text = match last {
            DataBlock::Text { text, .. } => text,
            _ => panic!("expected text block"),
        };
        assert!(text.starts_with("[duration: "), "got: {text}");
        assert!(!text.contains("[duration: 0ms]"), "got: {text}");
    }

    #[tokio::test]
    async fn duration_not_appended_by_default() {
        let called = Arc::new(AtomicBool::new(false));
        let t = HookedTool {
            inner: Arc::new(SpyTool {
                called,
                terminate: false,
            }),
            before: None,
            after: None,
        };
        let r = t.execute(serde_json::json!({}), &make_ctx()).await.unwrap();
        assert_eq!(r.model_content.len(), 1, "no duration block expected");
    }

    #[tokio::test]
    async fn duration_appended_on_failure() {
        let t = HookedTool {
            inner: Arc::new(TimedTool {
                fail: true,
                sleep_ms: 0,
            }),
            before: None,
            after: None,
        };
        let err = t
            .execute(serde_json::json!({}), &make_ctx())
            .await
            .unwrap_err();
        assert!(
            err.model_message.contains("\n[duration: "),
            "expected \\n-prefixed duration suffix, got: {}",
            err.model_message
        );
        assert!(
            err.model_message.ends_with(']'),
            "got: {}",
            err.model_message
        );
    }

    #[tokio::test]
    async fn duration_survives_hook_replace() {
        struct ReplaceHook;
        impl AfterToolCallHook for ReplaceHook {
            fn on_complete<'a>(
                &'a self,
                _: AfterToolCallCtx<'a>,
            ) -> BoxFuture<'a, AfterToolCallDecision> {
                Box::pin(async {
                    AfterToolCallDecision::Replace(Ok(ToolResult::full(
                        vec![DataBlock::text("replaced")],
                        serde_json::Value::Null,
                        false,
                    )))
                })
            }
        }
        let t = HookedTool {
            inner: Arc::new(TimedTool {
                fail: false,
                sleep_ms: 0,
            }),
            before: None,
            after: Some(Arc::new(ReplaceHook)),
        };
        let r = t.execute(serde_json::json!({}), &make_ctx()).await.unwrap();
        let blocks: Vec<&str> = r
            .model_content
            .iter()
            .filter_map(|b| match b {
                DataBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], "replaced");
        assert!(blocks[1].starts_with("[duration: "));
    }

    #[tokio::test]
    async fn ctx_duration_excludes_hook_overhead() {
        // hook 自身 sleep 250ms；ctx.duration 应只含 inner execute（~0ms）。
        // bound 125ms：污染（计时含 hook）时必然 ≥250ms 超界，正常调度
        // 抖动（µs 级）不可能触界。50ms/45ms 的旧边距在饱和 CI 上可被
        // 去调度击穿。
        struct SlowHook;
        impl AfterToolCallHook for SlowHook {
            fn on_complete<'a>(
                &'a self,
                ctx: AfterToolCallCtx<'a>,
            ) -> BoxFuture<'a, AfterToolCallDecision> {
                Box::pin(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    assert!(
                        ctx.duration < std::time::Duration::from_millis(125),
                        "ctx.duration should exclude hook time, got {:?}",
                        ctx.duration
                    );
                    AfterToolCallDecision::Passthrough
                })
            }
        }
        let t = HookedTool {
            inner: Arc::new(TimedTool {
                fail: false,
                sleep_ms: 0,
            }),
            before: None,
            after: Some(Arc::new(SlowHook)),
        };
        let _ = t.execute(serde_json::json!({}), &make_ctx()).await.unwrap();
    }

    #[tokio::test]
    async fn duration_appended_to_empty_model_content() {
        struct EmptyTool;
        impl Tool for EmptyTool {
            fn name(&self) -> &str {
                "empty"
            }
            fn description(&self) -> &str {
                "empty tool"
            }
            fn parameters_schema(&self) -> &serde_json::Value {
                static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
                S.get_or_init(|| serde_json::json!({}))
            }
            fn report_duration(&self) -> bool {
                true
            }
            fn execute<'a>(
                &'a self,
                _: serde_json::Value,
                _: &'a ToolContext,
            ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
                Box::pin(async { Ok(ToolResult::full(vec![], serde_json::Value::Null, false)) })
            }
        }
        let t = HookedTool {
            inner: Arc::new(EmptyTool),
            before: None,
            after: None,
        };
        let r = t.execute(serde_json::json!({}), &make_ctx()).await.unwrap();
        assert_eq!(r.model_content.len(), 1);
        let DataBlock::Text { text, .. } = &r.model_content[0] else {
            panic!("expected text block, got {:?}", r.model_content[0]);
        };
        assert!(text.starts_with("[duration: "), "got: {text}");
    }

    #[tokio::test]
    async fn duration_mirrored_into_session_projection() {
        // Projected: mirror appended to persisted content; Ephemeral: to
        // summary; Full: no mirror (materializes from model_content).
        struct ProjectedTool;
        impl Tool for ProjectedTool {
            fn name(&self) -> &str {
                "projected"
            }
            fn description(&self) -> &str {
                "projected tool"
            }
            fn parameters_schema(&self) -> &serde_json::Value {
                static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
                S.get_or_init(|| serde_json::json!({}))
            }
            fn report_duration(&self) -> bool {
                true
            }
            fn execute<'a>(
                &'a self,
                _: serde_json::Value,
                _: &'a ToolContext,
            ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
                Box::pin(async {
                    Ok(ToolResult::projected(
                        vec![DataBlock::text("body")],
                        vec![DataBlock::text("body")],
                        serde_json::Value::Null,
                        false,
                    ))
                })
            }
        }
        struct EphemeralTool;
        impl Tool for EphemeralTool {
            fn name(&self) -> &str {
                "ephemeral"
            }
            fn description(&self) -> &str {
                "ephemeral tool"
            }
            fn parameters_schema(&self) -> &serde_json::Value {
                static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
                S.get_or_init(|| serde_json::json!({}))
            }
            fn report_duration(&self) -> bool {
                true
            }
            fn execute<'a>(
                &'a self,
                _: serde_json::Value,
                _: &'a ToolContext,
            ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
                Box::pin(async {
                    Ok(ToolResult::ephemeral(
                        vec![DataBlock::text("body")],
                        "Fetched page",
                        serde_json::Value::Null,
                        false,
                    ))
                })
            }
        }
        for tool in [
            Arc::new(ProjectedTool) as Arc<dyn Tool>,
            Arc::new(EphemeralTool),
        ] {
            let t = HookedTool {
                inner: tool,
                before: None,
                after: None,
            };
            let r = t.execute(serde_json::json!({}), &make_ctx()).await.unwrap();
            let persisted = r.session_message("c1".into(), chrono::Utc::now()).unwrap();
            let last = persisted.content.last().unwrap();
            assert!(
                matches!(last, DataBlock::Text { text, .. } if text.contains("[duration: ")),
                "projection must carry duration for replay, got {last:?}"
            );
        }
    }

    #[tokio::test]
    async fn no_hooks_direct_passthrough() {
        let called = Arc::new(AtomicBool::new(false));
        let t = HookedTool {
            inner: Arc::new(SpyTool {
                called: called.clone(),
                terminate: false,
            }),
            before: None,
            after: None,
        };
        let ctx = make_ctx();
        let r = t.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(called.load(Ordering::SeqCst));
        assert!(!r.terminate);
    }

    #[tokio::test]
    async fn before_allow_original_args_passed() {
        struct AllowHook;
        impl BeforeToolCallHook for AllowHook {
            fn on_call<'a>(
                &'a self,
                _: BeforeToolCallCtx<'a>,
            ) -> BoxFuture<'a, BeforeToolCallDecision> {
                Box::pin(async { BeforeToolCallDecision::Allow })
            }
        }
        let called = Arc::new(AtomicBool::new(false));
        let t = HookedTool {
            inner: Arc::new(SpyTool {
                called: called.clone(),
                terminate: false,
            }),
            before: Some(Arc::new(AllowHook)),
            after: None,
        };
        let _ = t
            .execute(serde_json::json!({"x":1}), &make_ctx())
            .await
            .unwrap();
        assert!(called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn before_modify_changes_args() {
        use parking_lot::Mutex;
        let received_args = Arc::new(Mutex::new(serde_json::Value::Null));

        struct ModifyHook;
        impl BeforeToolCallHook for ModifyHook {
            fn on_call<'a>(
                &'a self,
                _: BeforeToolCallCtx<'a>,
            ) -> BoxFuture<'a, BeforeToolCallDecision> {
                Box::pin(async {
                    BeforeToolCallDecision::Modify(serde_json::json!({"modified": true}))
                })
            }
        }

        struct RecordArgsTool {
            received: Arc<Mutex<serde_json::Value>>,
        }
        impl Tool for RecordArgsTool {
            fn name(&self) -> &str {
                "record"
            }
            fn description(&self) -> &str {
                ""
            }
            fn parameters_schema(&self) -> &serde_json::Value {
                static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
                S.get_or_init(|| serde_json::json!({}))
            }
            fn execute<'a>(
                &'a self,
                args: serde_json::Value,
                _: &'a ToolContext,
            ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
                *self.received.lock() = args;
                Box::pin(async { Ok(ToolResult::full(vec![], serde_json::Value::Null, false)) })
            }
        }

        let t = HookedTool {
            inner: Arc::new(RecordArgsTool {
                received: received_args.clone(),
            }),
            before: Some(Arc::new(ModifyHook)),
            after: None,
        };
        let _ = t
            .execute(serde_json::json!({"original": true}), &make_ctx())
            .await
            .unwrap();
        assert_eq!(*received_args.lock(), serde_json::json!({"modified": true}));
    }

    #[tokio::test]
    async fn before_deny_inner_not_called() {
        let called = Arc::new(AtomicBool::new(false));
        struct DenyHook;
        impl BeforeToolCallHook for DenyHook {
            fn on_call<'a>(
                &'a self,
                _: BeforeToolCallCtx<'a>,
            ) -> BoxFuture<'a, BeforeToolCallDecision> {
                Box::pin(async {
                    BeforeToolCallDecision::Deny(ToolFailure::new("denied", "denied"))
                })
            }
        }
        let t = HookedTool {
            inner: Arc::new(SpyTool {
                called: called.clone(),
                terminate: false,
            }),
            before: Some(Arc::new(DenyHook)),
            after: None,
        };
        let failure = t
            .execute(serde_json::json!({}), &make_ctx())
            .await
            .unwrap_err();
        assert!(!called.load(Ordering::SeqCst));
        assert_eq!(failure.code, "denied");
    }

    #[tokio::test]
    async fn before_deny_still_invokes_after_hook_with_err() {
        // Denied calls must stay observable: the after-hook sees Err (with
        // the deny failure), the inner tool is never executed, and duration
        // is near-zero. Guards audit/tracing from missing deny events.
        struct DenyHook;
        impl BeforeToolCallHook for DenyHook {
            fn on_call<'a>(
                &'a self,
                _: BeforeToolCallCtx<'a>,
            ) -> BoxFuture<'a, BeforeToolCallDecision> {
                Box::pin(async {
                    BeforeToolCallDecision::Deny(ToolFailure::new("denied", "denied"))
                })
            }
        }
        struct ObserveAfter {
            seen: Arc<AtomicBool>,
            was_err: Arc<AtomicBool>,
            code: Arc<AtomicUsize>,
        }
        impl AfterToolCallHook for ObserveAfter {
            fn on_complete<'a>(
                &'a self,
                ctx: AfterToolCallCtx<'a>,
            ) -> BoxFuture<'a, AfterToolCallDecision> {
                self.seen.store(true, Ordering::SeqCst);
                if let Err(f) = ctx.result {
                    self.was_err.store(true, Ordering::SeqCst);
                    // Stable small hash of the code — atomic-friendly probe.
                    let h: usize = f.code.bytes().map(|b| b as usize).sum();
                    self.code.store(h, Ordering::SeqCst);
                }
                Box::pin(async { AfterToolCallDecision::Passthrough })
            }
        }
        let seen = Arc::new(AtomicBool::new(false));
        let was_err = Arc::new(AtomicBool::new(false));
        let code = Arc::new(AtomicUsize::new(0));
        let t = HookedTool {
            inner: Arc::new(SpyTool {
                called: Arc::new(AtomicBool::new(false)),
                terminate: false,
            }),
            before: Some(Arc::new(DenyHook)),
            after: Some(Arc::new(ObserveAfter {
                seen: seen.clone(),
                was_err: was_err.clone(),
                code: code.clone(),
            })),
        };
        let failure = t
            .execute(serde_json::json!({}), &make_ctx())
            .await
            .unwrap_err();
        assert_eq!(failure.code, "denied");
        assert!(seen.load(Ordering::SeqCst), "after-hook must see the deny");
        assert!(was_err.load(Ordering::SeqCst), "after-hook must see Err");
        let expected: usize = "denied".bytes().map(|b| b as usize).sum();
        assert_eq!(code.load(Ordering::SeqCst), expected);
    }

    #[tokio::test]
    async fn after_passthrough_result_unchanged() {
        struct PassthroughHook;
        impl AfterToolCallHook for PassthroughHook {
            fn on_complete<'a>(
                &'a self,
                _: AfterToolCallCtx<'a>,
            ) -> BoxFuture<'a, AfterToolCallDecision> {
                Box::pin(async { AfterToolCallDecision::Passthrough })
            }
        }
        let t = HookedTool {
            inner: Arc::new(SpyTool {
                called: Arc::new(AtomicBool::new(false)),
                terminate: false,
            }),
            before: None,
            after: Some(Arc::new(PassthroughHook)),
        };
        let r = t.execute(serde_json::json!({}), &make_ctx()).await.unwrap();
        if let DataBlock::Text { text, .. } = &r.model_content[0] {
            assert_eq!(text, "spy result");
        } else {
            panic!("unexpected");
        }
    }

    #[tokio::test]
    async fn after_replace_is_atomic() {
        struct PatchHook;
        impl AfterToolCallHook for PatchHook {
            fn on_complete<'a>(
                &'a self,
                _: AfterToolCallCtx<'a>,
            ) -> BoxFuture<'a, AfterToolCallDecision> {
                Box::pin(async {
                    AfterToolCallDecision::Replace(Ok(ToolResult::projected(
                        vec![DataBlock::text("replacement")],
                        vec![DataBlock::text("persisted replacement")],
                        serde_json::json!({"replaced": true}),
                        true,
                    )))
                })
            }
        }
        let t = HookedTool {
            inner: Arc::new(SpyTool {
                called: Arc::new(AtomicBool::new(false)),
                terminate: false,
            }),
            before: None,
            after: Some(Arc::new(PatchHook)),
        };
        let r = t.execute(serde_json::json!({}), &make_ctx()).await.unwrap();
        assert!(r.terminate);
        assert_eq!(r.details, serde_json::json!({"replaced": true}));
        assert!(matches!(
            r.session_projection,
            ToolSessionProjection::Projected { .. }
        ));
    }

    #[test]
    fn format_duration_threshold() {
        assert_eq!(format_duration(std::time::Duration::from_millis(0)), "0ms");
        assert_eq!(
            format_duration(std::time::Duration::from_millis(999)),
            "999ms"
        );
        assert_eq!(
            format_duration(std::time::Duration::from_millis(1000)),
            "1.00s"
        );
        assert_eq!(
            format_duration(std::time::Duration::from_millis(2350)),
            "2.35s"
        );
    }
}
