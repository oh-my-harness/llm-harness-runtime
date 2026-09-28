use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use axum::http::StatusCode;
use llm_harness_agent::harness::{AgentHarness, AgentHarnessOptions};
use llm_harness_platform::sandbox::Sandbox;
use llm_harness_sandbox_remote::{RemoteSandbox, RemoteSandboxClient, RemoteSandboxClientConfig};
use llm_harness_types::{AgentEvent, ExecutionEnv};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::auth::AuthenticatedPrincipal;
use crate::config::ControlServiceConfig;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlEvent {
    TextDelta { text: String },
    ToolStarted { name: String },
    ToolFinished { name: String, summary: String },
    RunFinished,
    Failed { message: String },
    Aborted,
    SessionClosed { reason: String },
}

#[derive(Debug, thiserror::Error)]
pub enum ControlServiceError {
    #[error("sandbox session {0} does not exist")]
    SessionNotFound(String),
    #[error("principal has reached its sandbox session limit")]
    LimitReached,
    #[error("agent is already running; prompt or abort first")]
    Busy,
    #[error("steer queue is full")]
    SteerQueueFull,
    #[error(transparent)]
    Sandbox(anyhow::Error),
}

impl ControlServiceError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::SessionNotFound(_) => StatusCode::NOT_FOUND,
            Self::LimitReached => StatusCode::TOO_MANY_REQUESTS,
            Self::Busy | Self::SteerQueueFull => StatusCode::CONFLICT,
            Self::Sandbox(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::SessionNotFound(_) => "agent_session_not_found",
            Self::LimitReached => "agent_session_limit_reached",
            Self::Busy => "agent_busy",
            Self::SteerQueueFull => "steer_queue_full",
            Self::Sandbox(_) => "sandbox_error",
        }
    }
}

#[async_trait]
pub trait AgentSandbox: Send + Sync {
    fn id(&self) -> String;
    fn backend(&self) -> String;
    fn env(&self) -> Arc<dyn ExecutionEnv>;
    async fn shutdown(self: Box<Self>) -> anyhow::Result<()>;
}

pub struct RemoteSandboxResource(RemoteSandbox);

impl RemoteSandboxResource {
    pub fn new(sandbox: RemoteSandbox) -> Self {
        Self(sandbox)
    }
}

#[async_trait]
impl AgentSandbox for RemoteSandboxResource {
    fn id(&self) -> String {
        self.0.sandbox_id().to_owned()
    }

    fn backend(&self) -> String {
        self.0.backend().to_owned()
    }

    fn env(&self) -> Arc<dyn ExecutionEnv> {
        Sandbox::env(&self.0)
    }

    async fn shutdown(self: Box<Self>) -> anyhow::Result<()> {
        Sandbox::shutdown(Box::new(self.0)).await
    }
}

pub struct AgentSessionComponents {
    pub harness: Arc<AgentHarness>,
    pub sandbox: Box<dyn AgentSandbox>,
}

#[async_trait]
pub trait AgentSessionFactory: Send + Sync {
    async fn create(
        &self,
        request: &CreateAgentSessionRequest,
    ) -> anyhow::Result<AgentSessionComponents>;
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAgentSessionRequest {
    pub model: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub trust_profile: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptRequest {
    pub prompt: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SteerRequest {
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct AgentSessionStatus {
    pub id: String,
    pub state: String,
    pub sandbox_id: String,
    pub backend: String,
    pub active_prompt: bool,
}

struct AgentSessionRecord {
    id: String,
    principal: String,
    harness: Arc<AgentHarness>,
    sandbox_id: String,
    backend: String,
    sandbox: std::sync::Mutex<Option<Box<dyn AgentSandbox>>>,
    active_tools: std::sync::Mutex<HashMap<String, String>>,
    events: broadcast::Sender<ControlEvent>,
    active_prompt: AtomicBool,
}

impl AgentSessionRecord {
    fn status(&self) -> AgentSessionStatus {
        let active_prompt = self.active_prompt.load(Ordering::SeqCst);
        AgentSessionStatus {
            id: self.id.clone(),
            state: if active_prompt { "running" } else { "idle" }.to_owned(),
            sandbox_id: self.sandbox_id.clone(),
            backend: self.backend.clone(),
            active_prompt,
        }
    }
}

#[derive(Clone)]
pub struct AgentControlManager {
    config: Arc<ControlServiceConfig>,
    factory: Arc<dyn AgentSessionFactory>,
    sessions: Arc<std::sync::Mutex<HashMap<String, Arc<AgentSessionRecord>>>>,
}

pub type SharedAgentControlManager = Arc<AgentControlManager>;

impl AgentControlManager {
    pub fn new(
        config: ControlServiceConfig,
        factory: Arc<dyn AgentSessionFactory>,
    ) -> anyhow::Result<Self> {
        if config.principals.is_empty() {
            anyhow::bail!("control service requires at least one principal");
        }
        for principal in &config.principals {
            if principal.principal.trim().is_empty() {
                anyhow::bail!("control-service principal id must not be empty");
            }
        }
        Ok(Self {
            config: Arc::new(config),
            factory,
            sessions: Arc::new(std::sync::Mutex::new(HashMap::new())),
        })
    }

    pub fn authenticate_token(&self, token: &str) -> Option<AuthenticatedPrincipal> {
        self.config.authenticate_token(token)
    }

    pub async fn create_session(
        &self,
        principal: AuthenticatedPrincipal,
        request: CreateAgentSessionRequest,
    ) -> Result<AgentSessionStatus, ControlServiceError> {
        let trust_profile = parse_trust_profile(request.trust_profile.as_deref())
            .map_err(ControlServiceError::Sandbox)?;
        if trust_profile > principal.max_trust_profile {
            return Err(ControlServiceError::Sandbox(anyhow::anyhow!(
                "requested trust profile exceeds the principal limit"
            )));
        }
        {
            let sessions = self.sessions.lock().expect("agent session registry");
            if sessions
                .values()
                .filter(|session| session.principal == principal.id)
                .count()
                >= principal.max_sessions
            {
                return Err(ControlServiceError::LimitReached);
            }
        }

        let components = self
            .factory
            .create(&request)
            .await
            .map_err(ControlServiceError::Sandbox)?;
        let id = Uuid::new_v4().to_string();
        let (events, _event_rx) = broadcast::channel(1024);
        let record = Arc::new(AgentSessionRecord {
            id: id.clone(),
            principal: principal.id,
            harness: components.harness,
            sandbox_id: components.sandbox.id(),
            backend: components.sandbox.backend(),
            sandbox: std::sync::Mutex::new(Some(components.sandbox)),
            active_tools: std::sync::Mutex::new(HashMap::new()),
            events,
            active_prompt: AtomicBool::new(false),
        });
        spawn_event_pump(Arc::clone(&record));
        self.sessions
            .lock()
            .expect("agent session registry")
            .insert(id, Arc::clone(&record));
        Ok(record.status())
    }

    pub fn get_session(
        &self,
        principal: &AuthenticatedPrincipal,
        session_id: &str,
    ) -> Result<AgentSessionStatus, ControlServiceError> {
        let session = self
            .find_owned_session(principal, session_id)?
            .ok_or_else(|| ControlServiceError::SessionNotFound(session_id.to_owned()))?;
        Ok(session.status())
    }

    pub fn subscribe_session(
        &self,
        principal: &AuthenticatedPrincipal,
        session_id: &str,
    ) -> Result<broadcast::Receiver<ControlEvent>, ControlServiceError> {
        let session = self
            .find_owned_session(principal, session_id)?
            .ok_or_else(|| ControlServiceError::SessionNotFound(session_id.to_owned()))?;
        Ok(session.events.subscribe())
    }

    pub fn prompt(
        &self,
        principal: &AuthenticatedPrincipal,
        session_id: &str,
        request: PromptRequest,
    ) -> Result<(String, AgentSessionStatus), ControlServiceError> {
        if request.prompt.trim().is_empty() {
            return Err(ControlServiceError::Sandbox(anyhow::anyhow!(
                "prompt must not be empty"
            )));
        }
        let session = self
            .find_owned_session(principal, session_id)?
            .ok_or_else(|| ControlServiceError::SessionNotFound(session_id.to_owned()))?;
        if session
            .active_prompt
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(ControlServiceError::Busy);
        }
        let prompt_id = Uuid::new_v4().to_string();
        let harness = Arc::clone(&session.harness);
        let events = session.events.clone();
        let record_for_task = Arc::clone(&session);
        tokio::spawn(async move {
            if let Err(error) = harness.prompt(request.prompt).await {
                let _ = events.send(ControlEvent::Failed {
                    message: error.to_string(),
                });
            }
            record_for_task.active_prompt.store(false, Ordering::SeqCst);
        });
        Ok((prompt_id, session.status()))
    }

    pub fn steer(
        &self,
        principal: &AuthenticatedPrincipal,
        session_id: &str,
        request: SteerRequest,
    ) -> Result<AgentSessionStatus, ControlServiceError> {
        if request.message.trim().is_empty() {
            return Err(ControlServiceError::Sandbox(anyhow::anyhow!(
                "steer message must not be empty"
            )));
        }
        let session = self
            .find_owned_session(principal, session_id)?
            .ok_or_else(|| ControlServiceError::SessionNotFound(session_id.to_owned()))?;
        session
            .harness
            .steer_message(llm_harness_types::AgentMessage::User(
                llm_harness_types::UserMessage {
                    content: vec![llm_harness_types::ContentBlock::Text {
                        text: request.message,
                    }],
                    timestamp: chrono::Utc::now(),
                },
            ))
            .map_err(|_| ControlServiceError::SteerQueueFull)?;
        Ok(session.status())
    }

    pub fn abort(
        &self,
        principal: &AuthenticatedPrincipal,
        session_id: &str,
    ) -> Result<(), ControlServiceError> {
        let session = self
            .find_owned_session(principal, session_id)?
            .ok_or_else(|| ControlServiceError::SessionNotFound(session_id.to_owned()))?;
        session.harness.abort();
        Ok(())
    }

    pub async fn delete_session(
        &self,
        principal: &AuthenticatedPrincipal,
        session_id: &str,
    ) -> Result<(), ControlServiceError> {
        let session = self
            .find_owned_session(principal, session_id)?
            .ok_or_else(|| ControlServiceError::SessionNotFound(session_id.to_owned()))?;
        {
            self.sessions
                .lock()
                .expect("agent session registry")
                .remove(session_id);
        }
        session.harness.abort();
        let _ = session.events.send(ControlEvent::SessionClosed {
            reason: "session deleted".to_owned(),
        });
        let sandbox = session.sandbox.lock().expect("sandbox lock").take();
        if let Some(sandbox) = sandbox {
            sandbox
                .shutdown()
                .await
                .map_err(ControlServiceError::Sandbox)?;
        }
        Ok(())
    }

    pub async fn shutdown_all(&self) -> anyhow::Result<()> {
        let sessions = {
            let mut sessions = self.sessions.lock().expect("agent session registry");
            std::mem::take(&mut *sessions)
        };
        let mut failures = 0;
        for session in sessions.into_values() {
            session.harness.abort();
            let _ = session.events.send(ControlEvent::SessionClosed {
                reason: "control service stopped".to_owned(),
            });
            let sandbox = session.sandbox.lock().expect("sandbox lock").take();
            if let Some(sandbox) = sandbox
                && sandbox.shutdown().await.is_err()
            {
                failures += 1;
            }
        }
        if failures == 0 {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "failed to shut down {failures} agent sessions"
            ))
        }
    }

    fn find_owned_session(
        &self,
        principal: &AuthenticatedPrincipal,
        session_id: &str,
    ) -> Result<Option<Arc<AgentSessionRecord>>, ControlServiceError> {
        let sessions = self.sessions.lock().expect("agent session registry");
        Ok(sessions
            .get(session_id)
            .filter(|session| session.principal == principal.id)
            .cloned())
    }
}

fn spawn_event_pump(session: Arc<AgentSessionRecord>) {
    let mut harness_rx = session.harness.subscribe();
    let events = session.events.clone();
    tokio::spawn(async move {
        loop {
            let event = match harness_rx.recv().await {
                Ok(event) => event,
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    let _ = events.send(ControlEvent::Failed {
                        message: format!("event stream lagged by {count} events"),
                    });
                    continue;
                }
            };
            let Some(control_event) = map_harness_event(
                event.as_ref(),
                &mut session.active_tools.lock().expect("active tool registry"),
            ) else {
                continue;
            };
            let _ = events.send(control_event);
        }
    });
}

fn map_harness_event(
    event: &llm_harness_agent::harness::AgentHarnessEvent,
    active_tools: &mut HashMap<String, String>,
) -> Option<ControlEvent> {
    if matches!(
        event,
        llm_harness_agent::harness::AgentHarnessEvent::Aborted
    ) {
        return Some(ControlEvent::Aborted);
    }
    let llm_harness_agent::harness::AgentHarnessEvent::Agent(agent_event) = event else {
        return None;
    };
    match agent_event {
        AgentEvent::TextDelta { text, .. } => Some(ControlEvent::TextDelta { text: text.clone() }),
        AgentEvent::ToolExecutionStart {
            tool_use_id,
            tool_name,
            ..
        } => {
            active_tools.insert(tool_use_id.clone(), tool_name.clone());
            Some(ControlEvent::ToolStarted {
                name: tool_name.clone(),
            })
        }
        AgentEvent::ToolExecutionEnd {
            tool_use_id,
            result,
        } => {
            let fallback_name = active_tools
                .remove(tool_use_id)
                .unwrap_or_else(|| "tool".to_owned());
            let (name, summary) = match result {
                Ok(tool_result) => {
                    let text = tool_result
                        .model_content
                        .iter()
                        .find_map(|block| match block {
                            llm_harness_types::DataBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    let summary: String = text.lines().next().unwrap_or_default().to_owned();
                    let summary = if summary.chars().count() > 120 {
                        let truncated: String = summary.chars().take(120).collect();
                        format!("{truncated}…")
                    } else {
                        summary
                    };
                    (fallback_name, summary)
                }
                Err(failure) => (fallback_name, failure.model_message.clone()),
            };
            Some(ControlEvent::ToolFinished { name, summary })
        }
        AgentEvent::AgentEnd { .. } => Some(ControlEvent::RunFinished),
        AgentEvent::Error(error) => Some(ControlEvent::Failed {
            message: error.to_string(),
        }),
        _ => None,
    }
}

fn parse_trust_profile(
    value: Option<&str>,
) -> anyhow::Result<llm_harness_platform::trust::TrustProfile> {
    match value {
        None | Some("") => Ok(llm_harness_platform::trust::TrustProfile::Restricted),
        Some("restricted") => Ok(llm_harness_platform::trust::TrustProfile::Restricted),
        Some("trusted-lan") => Ok(llm_harness_platform::trust::TrustProfile::TrustedLan),
        Some("trusted-full") => Ok(llm_harness_platform::trust::TrustProfile::TrustedFull),
        Some(other) => anyhow::bail!("invalid trust profile {other:?}"),
    }
}

pub struct RemoteSandboxSessionFactory {
    config: ControlServiceConfig,
}

impl RemoteSandboxSessionFactory {
    pub fn new(config: ControlServiceConfig) -> Self {
        Self { config }
    }

    fn sandbox_config(
        &self,
        trust_profile: llm_harness_platform::trust::TrustProfile,
    ) -> llm_harness_platform::sandbox::SandboxConfig {
        llm_harness_platform::sandbox::SandboxConfig {
            fs_allowlist: Vec::new(),
            fs_denylist: Vec::new(),
            net_allowlist: Vec::new(),
            resource_limits: llm_harness_platform::sandbox::ResourceLimits::default(),
            work_dir: None,
            trust_profile,
            host_binds: Vec::new(),
        }
    }

    fn llm_client(&self) -> anyhow::Result<Arc<dyn llm_adapter::Provider>> {
        let api_key = self.config.llm_api_key()?;
        let client: Arc<dyn llm_adapter::Provider> = match self.config.llm.provider.as_str() {
            "openai" => {
                let mut builder = llm_adapter::openai::OpenAIProvider::builder(api_key);
                if let Some(base_url) = &self.config.llm.base_url {
                    builder = builder.base_url(base_url.clone());
                }
                Arc::new(builder.build())
            }
            _ => {
                let mut builder = llm_adapter::anthropic::AnthropicProvider::builder(api_key);
                if let Some(base_url) = &self.config.llm.base_url {
                    builder = builder.base_url(base_url.clone());
                }
                Arc::new(builder.build())
            }
        };
        Ok(client)
    }
}

#[async_trait]
impl AgentSessionFactory for RemoteSandboxSessionFactory {
    async fn create(
        &self,
        request: &CreateAgentSessionRequest,
    ) -> anyhow::Result<AgentSessionComponents> {
        let trust_profile = parse_trust_profile(request.trust_profile.as_deref())?;
        let gateway_token = self.config.gateway_token()?;
        let client_config = if self.config.gateway.allow_insecure_http {
            RemoteSandboxClientConfig::new_insecure(
                self.config.gateway.base_url.clone(),
                gateway_token,
            )
        } else {
            RemoteSandboxClientConfig::new(self.config.gateway.base_url.clone(), gateway_token)
        }?;
        let client = Arc::new(RemoteSandboxClient::new(client_config)?);
        let sandbox = RemoteSandbox::connect(self.sandbox_config(trust_profile), client).await?;
        let env = Sandbox::env(&sandbox);
        let model = request
            .model
            .as_deref()
            .unwrap_or(&self.config.default_model);
        let mut options = AgentHarnessOptions::new(model);
        options.system_prompt = Some(request.system_prompt.clone().unwrap_or_else(|| {
            "You are a helpful agent working in the user's remote sandbox. Use tools to inspect \
             files and run commands; be concise."
                .to_owned()
        }));
        options.tools = vec![
            Arc::new(llm_harness_tools::BashTool::new()) as Arc<dyn llm_harness_types::Tool>,
            Arc::new(llm_harness_tools::ReadTool::new(None)),
            Arc::new(llm_harness_tools::WriteTool::new(None)),
            Arc::new(llm_harness_tools::EditTool::new(None)),
            Arc::new(llm_harness_tools::GlobTool::new()),
            Arc::new(llm_harness_tools::GrepTool::new()),
        ];
        let harness = Arc::new(AgentHarness::new_in_memory(self.llm_client()?, env, options).await);
        Ok(AgentSessionComponents {
            harness,
            sandbox: Box::new(RemoteSandboxResource(sandbox)),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use llm_harness_agent::harness::AgentHarnessOptions;
    use llm_harness_loop::LlmClient;
    use llm_harness_loop::test_utils::{MockLlmClient, MockResponse, NoOpEnv};

    use super::*;

    struct FakeSandbox {
        id: String,
        shutdown: Arc<AtomicBool>,
    }

    #[async_trait]
    impl AgentSandbox for FakeSandbox {
        fn id(&self) -> String {
            self.id.clone()
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

    struct FakeFactory {
        sandbox_shutdown: Arc<AtomicBool>,
    }

    #[async_trait]
    impl AgentSessionFactory for FakeFactory {
        async fn create(
            &self,
            _request: &CreateAgentSessionRequest,
        ) -> anyhow::Result<AgentSessionComponents> {
            let client: Arc<dyn LlmClient> =
                Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
            let env: Arc<dyn ExecutionEnv> = Arc::new(NoOpEnv);
            let harness =
                AgentHarness::new_in_memory(client, env, AgentHarnessOptions::new("test-model"))
                    .await;
            Ok(AgentSessionComponents {
                harness: Arc::new(harness),
                sandbox: Box::new(FakeSandbox {
                    id: "sandbox-1".to_owned(),
                    shutdown: Arc::clone(&self.sandbox_shutdown),
                }),
            })
        }
    }

    fn config(max_sessions: Option<usize>) -> ControlServiceConfig {
        ControlServiceConfig {
            bind: None,
            port: None,
            default_model: "test-model".to_owned(),
            gateway: crate::config::GatewayConfig {
                base_url: "http://127.0.0.1:1".to_owned(),
                token_env: "AGENT_CONTROL_TEST_GATEWAY".to_owned(),
                allow_insecure_http: true,
            },
            llm: crate::config::LlmConfig {
                api_key_env: "AGENT_CONTROL_TEST_API_KEY".to_owned(),
                provider: "test".to_owned(),
                base_url: None,
            },
            principals: vec![crate::config::PrincipalConfig {
                principal: "phone".to_owned(),
                token_env: "AGENT_CONTROL_TEST_TOKEN".to_owned(),
                max_trust_profile: llm_harness_platform::trust::TrustProfile::Restricted,
                max_sessions,
            }],
        }
    }

    fn principal(max_sessions: usize) -> AuthenticatedPrincipal {
        AuthenticatedPrincipal {
            id: "phone".to_owned(),
            max_trust_profile: llm_harness_platform::trust::TrustProfile::Restricted,
            max_sessions,
        }
    }

    #[tokio::test]
    async fn creates_reads_and_deletes_a_session() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let manager = AgentControlManager::new(
            config(None),
            Arc::new(FakeFactory {
                sandbox_shutdown: Arc::clone(&shutdown),
            }),
        )
        .unwrap();
        let status = manager
            .create_session(principal(1), CreateAgentSessionRequest::default())
            .await
            .unwrap();
        assert_eq!(status.state, "idle");
        assert_eq!(status.sandbox_id, "sandbox-1");
        assert_eq!(
            manager.get_session(&principal(16), &status.id).unwrap().id,
            status.id
        );

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
    async fn enforces_per_principal_session_limit() {
        let manager = AgentControlManager::new(
            config(Some(1)),
            Arc::new(FakeFactory {
                sandbox_shutdown: Arc::new(AtomicBool::new(false)),
            }),
        )
        .unwrap();
        manager
            .create_session(principal(16), CreateAgentSessionRequest::default())
            .await
            .unwrap();
        assert!(matches!(
            manager
                .create_session(principal(1), CreateAgentSessionRequest::default())
                .await,
            Err(ControlServiceError::LimitReached)
        ));
    }

    #[tokio::test]
    async fn prompt_streams_a_completed_turn() {
        let manager = AgentControlManager::new(
            config(None),
            Arc::new(FakeFactory {
                sandbox_shutdown: Arc::new(AtomicBool::new(false)),
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
        let (_, prompt_status) = manager
            .prompt(
                &principal(16),
                &status.id,
                PromptRequest {
                    prompt: "hello".to_owned(),
                },
            )
            .unwrap();
        assert_eq!(prompt_status.state, "running");

        let mut saw_finished = false;
        for _ in 0..8 {
            if let Ok(ControlEvent::RunFinished) = events.recv().await {
                saw_finished = true;
                break;
            }
        }
        assert!(saw_finished);
        assert_eq!(
            manager
                .get_session(&principal(16), &status.id)
                .unwrap()
                .state,
            "idle"
        );
    }
}
