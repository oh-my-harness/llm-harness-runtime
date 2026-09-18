//! 装配引擎：TeamSpec → AgentTeam。
//!
//! 纯装配逻辑：校验 toolkits、解析 ModelRef、解释 behaviors（Patrol 巡检
//! 文本生成、Handoff 路由表收集），产出可运行的团队。模板 persona 已在
//! [`crate::template_store`] 展开为全文，引擎只认 TeamSpec。

use std::sync::Arc;

use thiserror::Error;

use llm_harness_agent_team::{
    AgentTeam, InboxJournal, IndividualSpec, TeamConfig, TeamError, TimeoutJournal,
};
use llm_harness_loop::LlmClient;
use llm_harness_mcp::manager::McpManager;
use llm_harness_types::ExecutionEnv;
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolResult};

use crate::scout;
use crate::team_spec::{Behavior, MemberSpec, TeamSpec};

/// 合法 toolkit 包名；`social` 是保留名（装配时单独拒绝），
/// `mcp:<server>` 需要在 data_dir/.mcp.json 中有对应配置。
pub const VALID_TOOLKITS: [&str; 4] = ["fs", "web", "memory", "knowledge"];

#[derive(Debug, Error)]
pub enum AssembleError {
    #[error("invalid team id: {0}")]
    InvalidTeamId(String),
    #[error("duplicate member id: {0}")]
    DuplicateMember(String),
    #[error("invalid member id: {0}")]
    InvalidMemberId(String),
    #[error("member {member}: unknown toolkit {toolkit}; valid: {}", valid.join(", "))]
    UnknownToolkit {
        member: String,
        toolkit: String,
        valid: Vec<String>,
    },
    #[error("handoff on member {member} targets unknown member: {to}")]
    UnknownHandoffTarget { member: String, to: String },
    #[error("member {member}: MCP server '{server}' is not configured in {config_file}")]
    McpServerNotConfigured {
        member: String,
        server: String,
        config_file: String,
    },
    #[error("MCP server '{0}' failed to connect: {1}")]
    McpConnection(String, String),
    #[error("invalid MCP server name '{0}'")]
    InvalidMcpServerName(String),
    #[error("no API key configured; set one in the panel settings first")]
    NoProviders,
    #[error(transparent)]
    Team(#[from] TeamError),
    #[error(transparent)]
    InboxJournal(#[from] llm_harness_agent_team::InboxJournalError),
    #[error(transparent)]
    TimerJournal(#[from] llm_harness_agent_team::TimerJournalError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Patrol 行为的空任务占位替换：引擎生成的默认巡检文本。
pub fn default_patrol_task(issues_dir: &str, interval_secs: u64) -> String {
    scout::task_text(issues_dir, interval_secs, scout::PENDING_CAP)
}

/// 装配输入。`repo_path` 为 None 时（非 coding 类模板）用 data_dir 兜底
/// working dir。
pub struct AssembleInputs {
    pub spec: TeamSpec,
    pub settings: crate::settings::Settings,
    pub api_key: String,
    pub env: Arc<dyn ExecutionEnv>,
    pub data_dir: std::path::PathBuf,
    pub repo_path: Option<std::path::PathBuf>,
}

/// 装配产出：团队 + Handoff 路由表（routes 层确认 issue 时消费）。
pub struct Assembled {
    pub team: AgentTeam,
    pub handoffs: Vec<HandoffRoute>,
    /// Cancels MCP background tasks on drop; `None` when the team has no MCP toolkits.
    pub mcp_lifecycle: Option<McpManagerLifecycle>,
}

/// RAII guard for MCP manager background tasks.
pub struct McpManagerLifecycle {
    manager: Arc<McpManager>,
}

impl Drop for McpManagerLifecycle {
    fn drop(&mut self) {
        self.manager.cancel_background_tasks();
    }
}

/// 资源确认后的交接路由（由 Behavior::Handoff 收集）。
#[derive(Clone, Debug, PartialEq)]
pub struct HandoffRoute {
    pub resource: String,
    pub to: String,
    pub message_template: String,
}

/// 校验一个成员的 toolkits；保留名/未知包 fail fast。
pub(crate) fn validate_toolkits(m: &MemberSpec) -> Result<(), AssembleError> {
    for tk in &m.toolkits {
        if tk == "social" {
            return Err(AssembleError::UnknownToolkit {
                member: m.id.clone(),
                toolkit: tk.clone(),
                valid: VALID_TOOLKITS.iter().map(|s| s.to_string()).collect(),
            });
        }
        if let Some(server) = tk.strip_prefix("mcp:") {
            if server.is_empty()
                || server.contains('/')
                || server.contains('\\')
                || server.contains('\0')
            {
                return Err(AssembleError::InvalidMcpServerName(server.to_string()));
            }
            continue;
        }
        if !VALID_TOOLKITS.contains(&tk.as_str()) {
            return Err(AssembleError::UnknownToolkit {
                member: m.id.clone(),
                toolkit: tk.clone(),
                valid: VALID_TOOLKITS.iter().map(|s| s.to_string()).collect(),
            });
        }
    }
    Ok(())
}

/// Collect unique MCP server names and the member IDs that request them.
fn mcp_server_names(spec: &TeamSpec) -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = Vec::new();
    for member in &spec.members {
        for toolkit in &member.toolkits {
            if let Some(server) = toolkit.strip_prefix("mcp:") {
                let entry = entries.iter_mut().find(|(name, _)| name == server);
                match entry {
                    Some((_, members)) => {
                        if !members.contains(&member.id) {
                            members.push_str(&format!(", {}", member.id));
                        }
                    }
                    None => entries.push((server.to_string(), member.id.clone())),
                }
            }
        }
    }
    entries
}

/// Load MCP server configs from `.mcp.json` in the data directory.
fn load_mcp_configs(
    data_dir: &std::path::Path,
) -> Result<Option<llm_harness_mcp::config::McpConfigFile>, AssembleError> {
    let config_path = data_dir.join(".mcp.json");
    if !config_path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&config_path)?;
    let config: llm_harness_mcp::config::McpConfigFile =
        serde_json::from_str(&raw).map_err(|e| {
            AssembleError::McpConnection(
                String::new(),
                format!("failed to parse {}: {e}", config_path.display()),
            )
        })?;
    Ok(Some(config))
}

/// Lightweight MCP tool adapter routed through a shared manager.
struct McpStudioTool {
    full_name: String,
    description: String,
    parameters_schema: serde_json::Value,
    manager: Arc<McpManager>,
}

impl Tool for McpStudioTool {
    fn name(&self) -> &str {
        &self.full_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        &self.parameters_schema
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<ToolResult, ToolFailure>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let result = self
                .manager
                .call_tool(&self.full_name, args)
                .await
                .map_err(|_| {
                    ToolFailure::new("mcp_call_failed", "MCP tool communication failed.")
                })?;
            let content: Vec<DataBlock> = result
                .content
                .iter()
                .filter_map(|block| match block {
                    llm_harness_mcp::types::McpContent::Text { text, .. } => {
                        Some(DataBlock::text(text.clone()))
                    }
                    _ => None,
                })
                .collect();
            if result.is_error {
                let message = content
                    .iter()
                    .filter_map(|block| match block {
                        DataBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let mut failure = ToolFailure::new(
                    "mcp_tool_error",
                    if message.is_empty() {
                        "MCP tool execution failed."
                    } else {
                        &message
                    },
                );
                failure.details = result
                    .structured_content
                    .clone()
                    .unwrap_or(serde_json::Value::Null);
                return Err(failure);
            }
            Ok(ToolResult::full(
                content,
                result.structured_content.unwrap_or(serde_json::Value::Null),
                false,
            ))
        })
    }
}

/// Connect to required MCP servers and return (server_name, tools) pairs.
async fn prepare_mcp_tools(
    spec: &TeamSpec,
    data_dir: &std::path::Path,
) -> Result<(Vec<(String, Vec<Arc<dyn Tool>>)>, McpManagerLifecycle), AssembleError> {
    let server_entries = mcp_server_names(spec);
    if server_entries.is_empty() {
        // No MCP requested: return an empty lifecycle so `Assembled` compiles.
        // The guard's Drop will be a no-op because the manager has no connections.
        let manager = Arc::new(McpManager::new());
        return Ok((Vec::new(), McpManagerLifecycle { manager }));
    }

    let config_path_str = data_dir.join(".mcp.json").display().to_string();
    let config_file =
        load_mcp_configs(data_dir)?.ok_or_else(|| AssembleError::McpServerNotConfigured {
            member: server_entries[0].1.clone(),
            server: server_entries[0].0.clone(),
            config_file: config_path_str.clone(),
        })?;

    let configs: std::collections::HashMap<String, llm_harness_mcp::config::McpServerConfig> =
        server_entries
            .iter()
            .filter_map(|(name, _)| {
                config_file
                    .mcp_servers
                    .get(name)
                    .map(|c| (name.clone(), c.clone()))
            })
            .collect();

    for (name, member) in &server_entries {
        if !configs.contains_key(name) {
            return Err(AssembleError::McpServerNotConfigured {
                member: member.clone(),
                server: name.clone(),
                config_file: config_path_str.clone(),
            });
        }
    }

    let manager = McpManager::with_cache_path(data_dir.join(".mcp-lock.json"));
    manager
        .discover_and_connect(configs)
        .await
        .map_err(|e| AssembleError::McpConnection(server_entries[0].0.clone(), e.to_string()))?;

    let manager = Arc::new(manager);
    for (server, _) in &server_entries {
        let status = manager.get_connection_status(server);
        if status != llm_harness_mcp::manager::ConnectionStatus::Connected {
            return Err(AssembleError::McpConnection(
                server.clone(),
                format!("server did not connect (status: {status:?})"),
            ));
        }
    }
    let lifecycle = McpManagerLifecycle {
        manager: Arc::clone(&manager),
    };
    let exposed_tools = manager.get_tools();
    let mut result = Vec::new();
    for (server, _) in &server_entries {
        let tools: Vec<Arc<dyn Tool>> = exposed_tools
            .iter()
            .filter(|t| t.server_name == *server)
            .map(|t| {
                let adapter = McpStudioTool {
                    full_name: t.full_name.clone(),
                    description: t.tool_def.description.clone(),
                    parameters_schema: t.tool_def.input_schema.clone(),
                    manager: Arc::clone(&manager),
                };
                Arc::new(adapter) as Arc<dyn Tool>
            })
            .collect();
        result.push((server.clone(), tools));
    }
    Ok((result, lifecycle))
}

/// 成员级 provider 覆写 + 全局设置 → provider 列表。
/// 覆写有值的字段用覆写（api_key/base_url/model），否则继承全局。
pub fn build_member_providers(
    m: &MemberSpec,
    settings: &crate::settings::Settings,
    global_key: &str,
) -> Vec<(String, Arc<dyn LlmClient>)> {
    let key = m
        .provider_override
        .as_ref()
        .and_then(|o| o.api_key.clone())
        .filter(|k| !k.is_empty())
        .unwrap_or_else(|| global_key.to_string());
    if key.is_empty() {
        return vec![];
    }
    let base_url = m
        .provider_override
        .as_ref()
        .and_then(|o| o.base_url.clone())
        .unwrap_or_else(|| settings.base_url.clone());

    // 覆写 model 有值 → 只为它建 client；否则全局三档。
    let models: Vec<String> = match m.provider_override.as_ref().and_then(|o| o.model.clone()) {
        Some(mo) if !mo.is_empty() => vec![mo],
        _ => vec![
            settings.models.strong.clone(),
            settings.models.main.clone(),
            settings.models.cheap.clone(),
        ],
    };
    let mut out: Vec<(String, Arc<dyn LlmClient>)> = Vec::new();
    for model in models {
        if model.is_empty() || out.iter().any(|(m2, _)| m2 == &model) {
            continue;
        }
        let client: Arc<dyn LlmClient> = if base_url.is_empty() {
            Arc::new(llm_harness_loop::OpenAIProvider::builder(&key).build())
        } else {
            Arc::new(llm_harness_loop::openai_compatible(&key, &base_url))
        };
        out.push((model, client));
    }
    out
}

pub fn validate_spec(spec: &TeamSpec) -> Result<(), AssembleError> {
    if spec.id.is_empty() || spec.id == "." || spec.id == ".." {
        return Err(AssembleError::InvalidTeamId(spec.id.clone()));
    }
    if spec.id.contains(['/', '\\', '\0']) {
        return Err(AssembleError::InvalidTeamId(spec.id.clone()));
    }

    let mut seen = std::collections::HashSet::new();
    for m in &spec.members {
        if m.id.is_empty()
            || matches!(m.id.as_str(), "." | ".." | "operator" | "self" | "timer")
            || m.id.contains(['/', '\\', '\0'])
            || m.id.starts_with("reminder:")
        {
            return Err(AssembleError::InvalidMemberId(m.id.clone()));
        }
        if !seen.insert(m.id.as_str()) {
            return Err(AssembleError::DuplicateMember(m.id.clone()));
        }
    }
    for m in &spec.members {
        for behavior in &m.behaviors {
            if let Behavior::Handoff { to, .. } = behavior
                && !seen.contains(to.as_str())
            {
                return Err(AssembleError::UnknownHandoffTarget {
                    member: m.id.clone(),
                    to: to.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Handoff routes declared by the current spec.
pub fn handoff_routes(spec: &TeamSpec) -> Vec<HandoffRoute> {
    spec.members
        .iter()
        .flat_map(|m| {
            m.behaviors.iter().filter_map(|behavior| match behavior {
                Behavior::Handoff {
                    on,
                    to,
                    message_template,
                } => Some(HandoffRoute {
                    resource: match on {
                        crate::team_spec::Trigger::ResourceConfirmed { resource } => {
                            resource.clone()
                        }
                    },
                    to: to.clone(),
                    message_template: message_template.clone(),
                }),
                _ => None,
            })
        })
        .collect()
}

/// 装配一个团队。team.build 内部统一安装社交/memory/knowledge 插件
/// （团队级启用），engine 只做 spec → specs + providers 的映射。
pub async fn assemble(inputs: AssembleInputs) -> Result<Assembled, AssembleError> {
    validate_spec(&inputs.spec)?;
    if inputs.spec.members.is_empty() {
        return Err(AssembleError::Team(TeamError::NoMembers));
    }
    for m in &inputs.spec.members {
        validate_toolkits(m)?;
    }

    let (mcp_tools, mcp_lifecycle) = prepare_mcp_tools(&inputs.spec, &inputs.data_dir).await?;
    let mcp_by_server: std::collections::HashMap<&str, &[Arc<dyn Tool>]> = mcp_tools
        .iter()
        .map(|(server, tools)| (server.as_str(), tools.as_slice()))
        .collect();

    let project_dir = inputs.data_dir.clone();
    let inbox_journal = InboxJournal::open(project_dir.join("inbox.sqlite3"))?;
    let timer_journal = TimeoutJournal::open(project_dir.join("timers.sqlite3"))?;
    let config = TeamConfig {
        memory_root: project_dir.join("memory"),
        session_base_dir: project_dir.join("sessions"),
        // Studio templates describe direct peer-to-peer review and feedback
        // routes. Full-mesh contact setup needs one slot for the operator
        // plus every other member; retain the core default for tiny teams.
        max_contacts_per_agent: inputs.spec.members.len().max(20),
        inbox_journal: Some(Arc::new(inbox_journal)),
        timer_journal: Some(Arc::new(timer_journal)),
        ..TeamConfig::default()
    };
    std::fs::create_dir_all(project_dir.join("memory"))?;

    let mut team = AgentTeam::with_config(config);
    let mut handoffs = Vec::new();
    for m in &inputs.spec.members {
        let model = m
            .provider_override
            .as_ref()
            .and_then(|o| o.model.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| m.model.resolve(&inputs.settings.models));
        let spec = IndividualSpec::new(&m.id, &m.role_label, model)
            .with_prompt(m.persona.clone())
            .configure(|b| {
                let b = b.max_tokens(16384);
                // 成员级 toolkit：社交/memory/knowledge 由 team.build 统一装；
                // fs/web 是显式 opt-in 的成员级插件。
                let mut b = b;
                for tk in &m.toolkits {
                    if let Some(server) = tk.strip_prefix("mcp:") {
                        if let Some(tools) = mcp_by_server.get(server) {
                            for tool in tools.iter() {
                                b = b.tool(Arc::clone(tool));
                            }
                        }
                    } else {
                        b = match tk.as_str() {
                            "fs" => b.install(&llm_harness_tools::FsToolsPlugin::new(None)),
                            "web" => b.install(&llm_harness_tools::WebToolsPlugin::default()),
                            // memory/knowledge 团队级已装；此处 no-op。
                            "memory" | "knowledge" => b,
                            _ => b, // validate_toolkits 已拦截未知包
                        };
                    };
                }
                b
            });
        team = team.member(spec);
        for bh in &m.behaviors {
            if let Behavior::Handoff {
                on,
                to,
                message_template,
            } = bh
            {
                handoffs.push(HandoffRoute {
                    resource: match on {
                        crate::team_spec::Trigger::ResourceConfirmed { resource } => {
                            resource.clone()
                        }
                    },
                    to: to.clone(),
                    message_template: message_template.clone(),
                });
            }
        }
    }

    let mut providers: Vec<(String, Arc<dyn LlmClient>)> = inputs
        .spec
        .members
        .iter()
        .flat_map(|m| build_member_providers(m, &inputs.settings, &inputs.api_key))
        .collect();
    if providers.is_empty() {
        // 全局 key 未配置：回退进程级 provider factory（测试装 mock，
        // 二进制由启动时配置安装）。
        providers = crate::env_cfg::providers_from_factory();
    }
    if providers.is_empty() {
        return Err(AssembleError::NoProviders);
    }

    team.build(inputs.env.clone(), providers).await?;

    // AgentTeam intentionally seeds only operator contacts. Studio specs are
    // declarative team topologies, so make every declared peer addressable
    // before runners start; templates can then rely on reviewer→judge and
    // judge→planner without first delegating through an intermediary.
    let social_context = team.social_context().ok_or(TeamError::NotBuilt)?.clone();
    for sender in &inputs.spec.members {
        for receiver in &inputs.spec.members {
            if sender.id != receiver.id {
                social_context.ensure_contact(
                    &llm_harness_agent_team::AgentId::new(&sender.id),
                    &llm_harness_agent_team::AgentId::new(&receiver.id),
                );
            }
        }
    }

    // Patrol arming：task 为空 → 引擎生成默认文本（issues_dir 有值则拼入）。
    for m in &inputs.spec.members {
        for bh in &m.behaviors {
            if let Behavior::Patrol {
                interval_secs,
                task,
                issues_dir,
            } = bh
            {
                let text = if task.is_empty() {
                    let dir = issues_dir.clone().unwrap_or_else(|| {
                        project_dir.join("issues").to_string_lossy().to_string()
                    });
                    default_patrol_task(&dir, *interval_secs)
                } else {
                    task.clone()
                };
                if let Err(e) = team.submit(&m.id, &text) {
                    tracing::error!("patrol arming failed for {}: {e}", m.id);
                }
            }
        }
    }

    Ok(Assembled {
        team,
        handoffs,
        mcp_lifecycle: Some(mcp_lifecycle),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::ModelTier;
    use crate::team_spec::{ModelRef, ProviderOverride, Trigger};

    fn settings() -> crate::settings::Settings {
        crate::settings::Settings {
            models: ModelTier {
                strong: "m".into(),
                main: "m".into(),
                cheap: "m".into(),
            },
            scout_interval_secs: 3600,
            base_url: String::new(),
        }
    }

    fn member(id: &str, toolkits: &[&str], model: ModelRef) -> MemberSpec {
        MemberSpec {
            id: id.into(),
            persona: "worker persona".into(),
            role_label: "w".into(),
            model,
            toolkits: toolkits.iter().map(|s| s.to_string()).collect(),
            behaviors: vec![],
            provider_override: None,
            persona_file: None,
        }
    }

    fn spec(members: Vec<MemberSpec>) -> TeamSpec {
        TeamSpec {
            id: "t".into(),
            template: None,
            template_baseline: None,
            name: "T".into(),
            repo_path: None,
            members,
        }
    }

    fn inputs(s: TeamSpec) -> AssembleInputs {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let repo = std::env::temp_dir().join(format!("ct-eng-{}-{}", s.id, std::process::id()));
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let data = std::env::temp_dir().join(format!("ct-eng-data-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&data).unwrap();
        AssembleInputs {
            spec: s,
            settings: settings(),
            api_key: "k".into(),
            env: Arc::new(llm_harness_sandbox::OsEnv::new(&repo)),
            data_dir: data,
            repo_path: Some(repo),
        }
    }

    #[tokio::test]
    async fn unknown_toolkit_fails_fast_with_valid_list() {
        let s = spec(vec![member(
            "worker",
            &["fs", "bogus"],
            ModelRef::Explicit("m".into()),
        )]);
        match assemble(inputs(s.clone())).await {
            Err(AssembleError::UnknownToolkit {
                member,
                toolkit,
                valid,
            }) => {
                assert_eq!(member, "worker");
                assert_eq!(toolkit, "bogus");
                assert!(valid.contains(&"fs".to_string()));
            }
            Err(other) => panic!("wrong error: {other:?}"),
            Ok(_) => panic!("expected error, got assembled team"),
        }
    }

    #[tokio::test]
    async fn social_is_rejected_as_reserved() {
        let s = spec(vec![member(
            "worker",
            &["social"],
            ModelRef::Explicit("m".into()),
        )]);
        let res = assemble(inputs(s.clone())).await;
        assert!(matches!(res, Err(AssembleError::UnknownToolkit { .. })));
    }

    #[tokio::test]
    async fn assembles_team_with_explicit_model_and_fs() {
        let s = spec(vec![member(
            "worker",
            &["fs"],
            ModelRef::Explicit("m".into()),
        )]);
        let out = assemble(inputs(s.clone())).await.unwrap();
        let h = out
            .team
            .harness(&llm_harness_agent_team::AgentId::new("worker"))
            .unwrap();
        assert_eq!(h.state().model, "m");
        assert!(
            h.state()
                .system_prompt
                .as_deref()
                .unwrap()
                .contains("worker persona")
        );
    }

    #[tokio::test]
    async fn tier_resolution_uses_global_settings() {
        let s = spec(vec![member(
            "worker",
            &["fs"],
            ModelRef::Tier("cheap".into()),
        )]);
        let out = assemble(inputs(s.clone())).await.unwrap();
        let h = out
            .team
            .harness(&llm_harness_agent_team::AgentId::new("worker"))
            .unwrap();
        assert_eq!(h.state().model, "m");
    }

    #[tokio::test]
    async fn member_provider_override_changes_model() {
        let mut m = member("worker", &["fs"], ModelRef::Explicit("m".into()));
        m.provider_override = Some(ProviderOverride {
            api_key: None,
            base_url: None,
            model: Some("override-model".into()),
        });
        let s = spec(vec![m]);
        let out = assemble(inputs(s.clone())).await.unwrap();
        let h = out
            .team
            .harness(&llm_harness_agent_team::AgentId::new("worker"))
            .unwrap();
        assert_eq!(h.state().model, "override-model");
    }

    #[test]
    fn patrol_default_text_covers_contract() {
        // 引擎生成的默认巡检文本 = scout::task_text 契约（路径 + 自续约）。
        let text = default_patrol_task("/x/issues", 3600);
        assert!(text.contains("/x/issues"));
        assert!(text.contains("set_reminder"));
        assert!(text.contains("3600"));
    }

    #[tokio::test]
    async fn patrol_behavior_submits_first_task() {
        // Patrol 成员装配成功且首个巡检任务已提交（submit 成功即入队；
        // runner 消费是并发的，这里只验证提交路径不失败）。
        let mut m = member("scout", &["fs"], ModelRef::Explicit("m".into()));
        m.behaviors = vec![Behavior::Patrol {
            interval_secs: 3600,
            task: String::new(),
            issues_dir: Some("/x/issues".into()),
        }];
        let s = spec(vec![m]);
        let out = assemble(inputs(s.clone())).await.unwrap();
        // submit 失败会被 engine 记日志不致命——验证装配整体成功即可
        assert!(
            out.team
                .harness(&llm_harness_agent_team::AgentId::new("scout"))
                .is_some()
        );
    }

    #[tokio::test]
    async fn handoff_routes_collected() {
        let mut m = member("triage", &[], ModelRef::Explicit("m".into()));
        m.behaviors = vec![Behavior::Handoff {
            on: Trigger::ResourceConfirmed {
                resource: "issues".into(),
            },
            to: "planner".into(),
            message_template: "issue: {body}".into(),
        }];
        let s = spec(vec![
            m,
            member("planner", &[], ModelRef::Explicit("m".into())),
        ]);
        let out = assemble(inputs(s.clone())).await.unwrap();
        assert_eq!(out.handoffs.len(), 1);
        assert_eq!(out.handoffs[0].resource, "issues");
        assert_eq!(out.handoffs[0].to, "planner");
        assert_eq!(out.handoffs[0].message_template, "issue: {body}");
    }

    #[test]
    fn invalid_member_id_rejects_unsafe_values() {
        for id in ["a/b", "a\\b", ".", "..", "operator", "reminder:x"] {
            let s = spec(vec![member(id, &[], ModelRef::Explicit("m".into()))]);
            assert!(matches!(
                validate_spec(&s),
                Err(AssembleError::InvalidMemberId(member_id)) if member_id == id
            ));
        }
    }

    #[test]
    fn invalid_team_id_fails_fast() {
        for id in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
            let mut s = spec(vec![member("w", &[], ModelRef::Explicit("m".into()))]);
            s.id = id.into();
            assert!(matches!(
                validate_spec(&s),
                Err(AssembleError::InvalidTeamId(_))
            ));
        }
    }

    #[test]
    fn duplicate_member_fails_fast() {
        let s = spec(vec![
            member("worker", &[], ModelRef::Explicit("m".into())),
            member("worker", &[], ModelRef::Explicit("m".into())),
        ]);
        let err = validate_spec(&s).unwrap_err();
        assert!(matches!(err, AssembleError::DuplicateMember(member_id) if member_id == "worker"));
    }

    #[test]
    fn unknown_handoff_target_fails_fast() {
        let mut m = member("triage", &[], ModelRef::Explicit("m".into()));
        m.behaviors = vec![Behavior::Handoff {
            on: Trigger::ResourceConfirmed {
                resource: "issues".into(),
            },
            to: "missing".into(),
            message_template: "issue: {body}".into(),
        }];
        let s = spec(vec![m]);
        let err = validate_spec(&s).unwrap_err();
        assert!(matches!(err, AssembleError::UnknownHandoffTarget { to, .. } if to == "missing"));
    }

    #[test]
    fn mcp_toolkit_valid_server_passes_validation() {
        let s = spec(vec![member(
            "w",
            &["mcp:my-mcp"],
            ModelRef::Explicit("m".into()),
        )]);
        validate_toolkits(&s.members[0]).unwrap();
    }

    #[test]
    fn mcp_toolkit_empty_server_rejected() {
        let s = spec(vec![member("w", &["mcp:"], ModelRef::Explicit("m".into()))]);
        let err = validate_toolkits(&s.members[0]).unwrap_err();
        assert!(matches!(err, AssembleError::InvalidMcpServerName(_)));
    }

    #[test]
    fn mcp_toolkit_path_traversal_rejected() {
        for bad in ["mcp:../etc", "mcp:a/b", "mcp:a\\b", "mcp:a\0b"] {
            let s = spec(vec![member("w", &[bad], ModelRef::Explicit("m".into()))]);
            let err = validate_toolkits(&s.members[0]).unwrap_err();
            assert!(
                matches!(err, AssembleError::InvalidMcpServerName(_)),
                "{bad}"
            );
        }
    }

    #[tokio::test]
    async fn mcp_without_config_file_is_error() {
        let s = spec(vec![member(
            "w",
            &["mcp:my-mcp"],
            ModelRef::Explicit("m".into()),
        )]);
        let res = assemble(inputs(s)).await;
        match res {
            Err(AssembleError::McpServerNotConfigured { server, .. }) => {
                assert_eq!(server, "my-mcp");
            }
            Ok(_) => panic!("expected McpServerNotConfigured, got assembled team"),
            Err(other) => panic!("expected McpServerNotConfigured, got {other}"),
        }
    }

    #[tokio::test]
    async fn mcp_server_missing_from_config_is_error() {
        let i = inputs(spec(vec![member(
            "w",
            &["mcp:my-mcp"],
            ModelRef::Explicit("m".into()),
        )]));
        std::fs::write(
            i.data_dir.join(".mcp.json"),
            r#"{"mcpServers":{"other":{"command":"true"}}}"#,
        )
        .unwrap();
        let res = assemble(i).await;
        match res {
            Err(AssembleError::McpServerNotConfigured { server, .. }) => {
                assert_eq!(server, "my-mcp");
            }
            Ok(_) => panic!("expected McpServerNotConfigured, got assembled team"),
            Err(other) => panic!("expected McpServerNotConfigured, got {other}"),
        }
    }

    #[test]
    fn mcp_server_names_deduplicated() {
        let s = spec(vec![
            member("a", &["mcp:x", "fs"], ModelRef::Tier("main".into())),
            member("b", &["mcp:x", "mcp:y"], ModelRef::Tier("main".into())),
        ]);
        let names: Vec<String> = mcp_server_names(&s)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, vec!["x".to_string(), "y".to_string()]);
    }
}
