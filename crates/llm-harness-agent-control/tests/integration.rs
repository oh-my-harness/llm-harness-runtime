//! Integration tests for the Agent Control Service.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use llm_harness_agent::harness::{AgentHarness, AgentHarnessOptions};
use llm_harness_loop::LlmClient;
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse, NoOpEnv};
use llm_harness_types::{ExecutionEnv, Tool, ToolContext, ToolFailure, ToolResult};

use llm_harness_agent_control::{
    AgentControlManager, AgentSandbox, AgentSessionComponents, AgentSessionFactory, ControlEvent,
    ControlServiceConfig, ControlServiceError, CreateAgentSessionRequest, GatewayConfig, LlmConfig,
    PrincipalConfig, PromptRequest, SessionEvent, SteerRequest,
};

struct FakeSandbox {
    shutdown: Arc<AtomicBool>,
}

#[async_trait]
impl AgentSandbox for FakeSandbox {
    fn id(&self) -> String {
        "test-sandbox".to_owned()
    }

    fn backend(&self) -> String {
        "test".to_owned()
    }

    fn env(&self) -> Arc<dyn ExecutionEnv> {
        Arc::new(NoOpEnv)
    }

    async fn shutdown(self: Box<Self>) -> anyhow::Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        Ok(())
    }
}

struct BlockingTool {
    ready: Arc<tokio::sync::Notify>,
}

impl Tool for BlockingTool {
    fn name(&self) -> &str {
        "blocking"
    }

    fn description(&self) -> &str {
        "blocks until aborted"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| serde_json::json!({"type": "object", "properties": {}}))
    }

    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> futures::future::BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        let ready = self.ready.clone();
        Box::pin(async move {
            ready.notify_one();
            ctx.abort.cancelled().await;
            Err(ToolFailure::aborted())
        })
    }
}

struct TextFactory {
    shutdown: Arc<AtomicBool>,
    create_count: AtomicUsize,
}

struct RecordedTextFactory {
    shutdown: Arc<AtomicBool>,
    client: Arc<MockLlmClient>,
}

#[async_trait]
impl AgentSessionFactory for RecordedTextFactory {
    async fn create(
        &self,
        _request: &CreateAgentSessionRequest,
    ) -> anyhow::Result<AgentSessionComponents> {
        let client: Arc<dyn LlmClient> = self.client.clone();
        let env: Arc<dyn ExecutionEnv> = Arc::new(NoOpEnv);
        let harness =
            AgentHarness::new_in_memory(client, env, AgentHarnessOptions::new("test-model")).await;
        Ok(AgentSessionComponents {
            harness: Arc::new(harness),
            sandbox: Box::new(FakeSandbox {
                shutdown: Arc::clone(&self.shutdown),
            }),
        })
    }
}

#[async_trait]
impl AgentSessionFactory for TextFactory {
    async fn create(
        &self,
        _request: &CreateAgentSessionRequest,
    ) -> anyhow::Result<AgentSessionComponents> {
        self.create_count.fetch_add(1, Ordering::SeqCst);
        let client: Arc<dyn LlmClient> =
            Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
        let env: Arc<dyn ExecutionEnv> = Arc::new(NoOpEnv);
        let harness =
            AgentHarness::new_in_memory(client, env, AgentHarnessOptions::new("test-model")).await;
        Ok(AgentSessionComponents {
            harness: Arc::new(harness),
            sandbox: Box::new(FakeSandbox {
                shutdown: Arc::clone(&self.shutdown),
            }),
        })
    }
}

struct BlockingToolFactory {
    ready: Arc<tokio::sync::Notify>,
    shutdown: Arc<AtomicBool>,
}

#[async_trait]
impl AgentSessionFactory for BlockingToolFactory {
    async fn create(
        &self,
        _request: &CreateAgentSessionRequest,
    ) -> anyhow::Result<AgentSessionComponents> {
        let client: Arc<dyn LlmClient> =
            Arc::new(MockLlmClient::new(vec![MockResponse::tool_use(
                "call_1", "blocking", "{}",
            )]));
        let env: Arc<dyn ExecutionEnv> = Arc::new(NoOpEnv);
        let mut opts = AgentHarnessOptions::new("test-model");
        opts.tools = vec![Arc::new(BlockingTool {
            ready: Arc::clone(&self.ready),
        }) as Arc<dyn Tool>];
        let harness = AgentHarness::new_in_memory(client, env, opts).await;
        Ok(AgentSessionComponents {
            harness: Arc::new(harness),
            sandbox: Box::new(FakeSandbox {
                shutdown: Arc::clone(&self.shutdown),
            }),
        })
    }
}

fn config(max_sessions: Option<usize>) -> ControlServiceConfig {
    ControlServiceConfig {
        bind: None,
        port: None,
        default_model: "test-model".to_owned(),
        gateway: GatewayConfig {
            base_url: "http://127.0.0.1:1".to_owned(),
            token_env: "unused".to_owned(),
            allow_insecure_http: true,
        },
        llm: LlmConfig {
            api_key_env: "unused".to_owned(),
            provider: "test".to_owned(),
            base_url: None,
        },
        principals: vec![PrincipalConfig {
            principal: "phone".to_owned(),
            token_env: "unused".to_owned(),
            max_trust_profile: llm_harness_platform::trust::TrustProfile::Restricted,
            max_sessions,
        }],
        persistence_path: None,
    }
}

fn principal(max_sessions: usize) -> llm_harness_agent_control::AuthenticatedPrincipal {
    llm_harness_agent_control::AuthenticatedPrincipal {
        id: "phone".to_owned(),
        max_trust_profile: llm_harness_platform::trust::TrustProfile::Restricted,
        max_sessions,
    }
}

#[tokio::test]
async fn full_flow_create_prompt_events_delete() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let manager = AgentControlManager::new(
        config(None),
        Arc::new(TextFactory {
            shutdown: Arc::clone(&shutdown),
            create_count: AtomicUsize::new(0),
        }),
    )
    .unwrap();

    let status = manager
        .create_session(principal(16), CreateAgentSessionRequest::default())
        .await
        .unwrap();
    assert_eq!(status.state, "idle");

    let mut events = manager
        .subscribe_session(&principal(16), &status.id)
        .unwrap();

    let (prompt_id, prompt_status) = manager
        .prompt(
            &principal(16),
            &status.id,
            PromptRequest {
                prompt: "hello".to_owned(),
            },
        )
        .unwrap();
    assert_eq!(prompt_status.state, "running");

    let mut saw_text = false;
    let mut saw_finished = false;
    let mut saw_turn_id = false;
    for _ in 0..32 {
        match events.recv().await {
            Ok(SessionEvent {
                prompt_id: Some(event_prompt_id),
                event: ControlEvent::TextDelta { .. },
                ..
            }) => {
                assert_eq!(event_prompt_id, prompt_id);
                saw_text = true;
            }
            Ok(SessionEvent {
                prompt_id: Some(event_prompt_id),
                event: ControlEvent::RunFinished,
                ..
            }) => {
                assert_eq!(event_prompt_id, prompt_id);
                saw_finished = true;
                break;
            }
            Ok(SessionEvent {
                prompt_id: Some(event_prompt_id),
                event: ControlEvent::TurnStarted { turn_id: event_turn_id },
                turn_id,
            }) => {
                assert_eq!(event_prompt_id, prompt_id);
                assert_eq!(turn_id.as_deref(), Some(event_turn_id.as_str()));
                assert!(!event_turn_id.is_empty());
                saw_turn_id = true;
            }
            Ok(SessionEvent {
                prompt_id: Some(event_prompt_id),
                event: ControlEvent::RunStarted,
                ..
            }) => {
                assert_eq!(event_prompt_id, prompt_id);
            }
            Ok(other) => panic!("unexpected event: {other:?}"),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(e) => panic!("event channel error: {e}"),
        }
    }
    assert!(saw_text, "should receive text delta");
    assert!(saw_finished, "should receive run finished");
    assert!(saw_turn_id, "assistant events should carry a turn id");

    let idle = manager.get_session(&principal(16), &status.id).unwrap();
    assert_eq!(idle.state, "idle");

    manager
        .delete_session(&principal(16), &status.id)
        .await
        .unwrap();
    assert!(shutdown.load(Ordering::SeqCst));
    assert!(matches!(
        manager.get_session(&principal(16), &status.id),
        Err(ControlServiceError::SessionNotFound(_))
    ));
}

#[tokio::test]
async fn idle_steer_is_queued_for_next_prompt() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let client = Arc::new(MockLlmClient::with_single(MockResponse::text("ok")));
    let factory = Arc::new(RecordedTextFactory {
        shutdown: Arc::clone(&shutdown),
        client: Arc::clone(&client),
    });
    let manager = AgentControlManager::new(config(None), factory).unwrap();
    let status = manager
        .create_session(principal(16), CreateAgentSessionRequest::default())
        .await
        .unwrap();

    manager
        .steer(
            &principal(16),
            &status.id,
            SteerRequest {
                message: "queued instruction".to_owned(),
            },
        )
        .unwrap();
    manager
        .prompt(
            &principal(16),
            &status.id,
            PromptRequest {
                prompt: "main prompt".to_owned(),
            },
        )
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        client.call_count.load(Ordering::SeqCst),
        1,
        "prompt should execute once"
    );
    let captured = client.captured_requests.lock().pop().unwrap();
    let request = format!("{captured:?}");
    assert!(request.contains("queued instruction"));
    assert!(request.contains("main prompt"));
}

#[tokio::test]
async fn active_steer_is_forwarded_to_harness() {
    let ready = Arc::new(tokio::sync::Notify::new());
    let shutdown = Arc::new(AtomicBool::new(false));
    let manager = AgentControlManager::new(
        config(None),
        Arc::new(BlockingToolFactory {
            ready: Arc::clone(&ready),
            shutdown: Arc::clone(&shutdown),
        }),
    )
    .unwrap();
    let status = manager
        .create_session(principal(16), CreateAgentSessionRequest::default())
        .await
        .unwrap();

    manager
        .prompt(
            &principal(16),
            &status.id,
            PromptRequest {
                prompt: "call blocking".to_owned(),
            },
        )
        .unwrap();
    ready.notified().await;
    let status = manager
        .steer(
            &principal(16),
            &status.id,
            SteerRequest {
                message: "change direction".to_owned(),
            },
        )
        .unwrap();
    assert_eq!(status.state, "running");

    manager.abort(&principal(16), &status.id).unwrap();
    manager
        .delete_session(&principal(16), &status.id)
        .await
        .unwrap();
}

#[tokio::test]
async fn abort_emits_aborted_event_and_clears_active_prompt() {
    let ready = Arc::new(tokio::sync::Notify::new());
    let shutdown = Arc::new(AtomicBool::new(false));
    let manager = AgentControlManager::new(
        config(None),
        Arc::new(BlockingToolFactory {
            ready: Arc::clone(&ready),
            shutdown: Arc::clone(&shutdown),
        }),
    )
    .unwrap();

    let status = manager
        .create_session(principal(16), CreateAgentSessionRequest::default())
        .await
        .unwrap();
    let mut events = manager
        .subscribe_session(&principal(16), &status.id)
        .unwrap();

    manager
        .prompt(
            &principal(16),
            &status.id,
            PromptRequest {
                prompt: "call blocking".to_owned(),
            },
        )
        .unwrap();

    ready.notified().await;
    manager.abort(&principal(16), &status.id).unwrap();

    let mut saw_aborted = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, events.recv()).await {
        if matches!(event.event, ControlEvent::Aborted) {
            saw_aborted = true;
            break;
        }
    }
    assert!(saw_aborted, "should receive Aborted event");

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let idle = manager.get_session(&principal(16), &status.id).unwrap();
    assert_eq!(idle.state, "idle");
    manager
        .delete_session(&principal(16), &status.id)
        .await
        .unwrap();
}

#[tokio::test]
async fn concurrent_prompt_same_session_returns_busy() {
    let ready = Arc::new(tokio::sync::Notify::new());
    let shutdown = Arc::new(AtomicBool::new(false));
    let manager = AgentControlManager::new(
        config(None),
        Arc::new(BlockingToolFactory {
            ready: Arc::clone(&ready),
            shutdown: Arc::clone(&shutdown),
        }),
    )
    .unwrap();

    let status = manager
        .create_session(principal(16), CreateAgentSessionRequest::default())
        .await
        .unwrap();

    let first = manager
        .prompt(
            &principal(16),
            &status.id,
            PromptRequest {
                prompt: "blocking".to_owned(),
            },
        )
        .unwrap();
    assert_eq!(first.1.state, "running");

    ready.notified().await;

    let second = manager.prompt(
        &principal(16),
        &status.id,
        PromptRequest {
            prompt: "again".to_owned(),
        },
    );
    assert!(
        matches!(second, Err(ControlServiceError::Busy)),
        "second prompt while running should be Busy, got: {second:?}"
    );

    manager.abort(&principal(16), &status.id).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    manager
        .delete_session(&principal(16), &status.id)
        .await
        .unwrap();
}

#[tokio::test]
async fn session_limit_blocks_excess_creates() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let manager = Arc::new(
        AgentControlManager::new(
            config(Some(1)),
            Arc::new(TextFactory {
                shutdown: Arc::clone(&shutdown),
                create_count: AtomicUsize::new(0),
            }),
        )
        .unwrap(),
    );

    let p = principal(1);
    let (s1, s2, s3) = tokio::join!(
        manager.create_session(p.clone(), CreateAgentSessionRequest::default()),
        manager.create_session(p.clone(), CreateAgentSessionRequest::default()),
        manager.create_session(p.clone(), CreateAgentSessionRequest::default()),
    );

    let results = vec![s1, s2, s3];
    let successes = results.iter().filter(|r| r.is_ok()).count();
    let limits = results
        .iter()
        .filter(|r| matches!(r, Err(ControlServiceError::LimitReached)))
        .count();
    assert_eq!(successes, 1, "only 1 create should succeed with limit 1");
    assert_eq!(limits, 2, "2 creates should be rejected with LimitReached");

    for result in results.into_iter().flatten() {
        let _ = manager.delete_session(&p, &result.id).await;
    }
}

#[tokio::test]
async fn cross_principal_isolation() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let manager = AgentControlManager::new(
        config(None),
        Arc::new(TextFactory {
            shutdown: Arc::clone(&shutdown),
            create_count: AtomicUsize::new(0),
        }),
    )
    .unwrap();

    let owner = principal(16);
    let other = llm_harness_agent_control::AuthenticatedPrincipal {
        id: "other-user".to_owned(),
        max_trust_profile: llm_harness_platform::trust::TrustProfile::Restricted,
        max_sessions: 16,
    };

    let status = manager
        .create_session(owner.clone(), CreateAgentSessionRequest::default())
        .await
        .unwrap();

    assert!(matches!(
        manager.get_session(&other, &status.id),
        Err(ControlServiceError::SessionNotFound(_))
    ));
    assert!(matches!(
        manager.prompt(
            &other,
            &status.id,
            PromptRequest {
                prompt: "hi".to_owned(),
            },
        ),
        Err(ControlServiceError::SessionNotFound(_))
    ));
    assert!(matches!(
        manager.abort(&other, &status.id),
        Err(ControlServiceError::SessionNotFound(_))
    ));

    manager.delete_session(&owner, &status.id).await.unwrap();
}
