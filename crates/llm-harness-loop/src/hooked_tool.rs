use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::*;

/// Decorates a `Tool` with optional before/after hooks. Stateless.
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

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            // Apply before-hook
            let effective_args = if let Some(h) = &self.before {
                let before_ctx = BeforeToolCallCtx {
                    run: &ctx.run,
                    assistant_message: &ctx.assistant_message,
                    tool_use_id: &ctx.tool_use_id,
                    tool_name: self.inner.name(),
                    args: &args,
                    turn_index: ctx.turn_index,
                };
                match h.on_call(before_ctx).await {
                    BeforeToolCallDecision::Allow => args,
                    BeforeToolCallDecision::Modify(new_args) => new_args,
                    BeforeToolCallDecision::Deny(failure) => return Err(failure),
                }
            } else {
                args
            };

            let result = self.inner.execute(effective_args.clone(), ctx).await;

            // Apply after-hook
            if let Some(h) = &self.after {
                let after_ctx = AfterToolCallCtx {
                    run: &ctx.run,
                    assistant_message: &ctx.assistant_message,
                    tool_use_id: &ctx.tool_use_id,
                    tool_name: self.inner.name(),
                    args: &effective_args,
                    result: &result,
                    turn_index: ctx.turn_index,
                };
                return match h.on_complete(after_ctx).await {
                    AfterToolCallDecision::Passthrough => result,
                    AfterToolCallDecision::Replace(replacement) => replacement,
                };
            }

            result
        })
    }
}

#[cfg(test)]
#[cfg(feature = "test-utils")]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
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
        use std::sync::Mutex;
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
                *self.received.lock().unwrap() = args;
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
        assert_eq!(
            *received_args.lock().unwrap(),
            serde_json::json!({"modified": true})
        );
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
}
