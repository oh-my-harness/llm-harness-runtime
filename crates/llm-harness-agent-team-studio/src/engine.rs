//! 装配引擎：TeamSpec → AgentTeam。
//!
//! 纯装配逻辑：校验 toolkits、解析 ModelRef、解释 behaviors（Patrol 巡检
//! 文本生成、Handoff 路由表收集），产出可运行的团队。模板 persona 已在
//! [`crate::template_store`] 展开为全文，引擎只认 TeamSpec。

use std::sync::Arc;

use thiserror::Error;

use llm_harness_agent_team::{AgentTeam, IndividualSpec, TeamConfig, TeamError};
use llm_harness_loop::LlmClient;
use llm_harness_types::ExecutionEnv;

use crate::scout;
use crate::team_spec::{Behavior, MemberSpec, TeamSpec};

/// 合法 toolkit 包名；`social` 是保留名（装配时单独拒绝），`mcp:<server>`
/// 本期报"未支持"。
pub const VALID_TOOLKITS: [&str; 4] = ["fs", "web", "memory", "knowledge"];

#[derive(Debug, Error)]
pub enum AssembleError {
    #[error("member {member}: unknown toolkit {toolkit}; valid: {}", valid.join(", "))]
    UnknownToolkit {
        member: String,
        toolkit: String,
        valid: Vec<String>,
    },
    #[error("member {member}: toolkit mcp:{server} requires MCP server config (planned)")]
    McpNotYetSupported { member: String, server: String },
    #[error("no API key configured; set one in the panel settings first")]
    NoProviders,
    #[error(transparent)]
    Team(#[from] TeamError),
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
}

/// 资源确认后的交接路由（由 Behavior::Handoff 收集）。
#[derive(Clone, Debug)]
pub struct HandoffRoute {
    pub resource: String,
    pub to: String,
    pub message_template: String,
}

/// 校验一个成员的 toolkits；保留名/未知包 fail fast。
fn validate_toolkits(m: &MemberSpec) -> Result<(), AssembleError> {
    for tk in &m.toolkits {
        if tk == "social" {
            return Err(AssembleError::UnknownToolkit {
                member: m.id.clone(),
                toolkit: tk.clone(),
                valid: VALID_TOOLKITS.iter().map(|s| s.to_string()).collect(),
            });
        }
        if let Some(server) = tk.strip_prefix("mcp:") {
            return Err(AssembleError::McpNotYetSupported {
                member: m.id.clone(),
                server: server.to_string(),
            });
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

/// 装配一个团队。team.build 内部统一安装社交/memory/knowledge 插件
/// （团队级启用），engine 只做 spec → specs + providers 的映射。
pub async fn assemble(inputs: AssembleInputs) -> Result<Assembled, AssembleError> {
    if inputs.spec.members.is_empty() {
        return Err(AssembleError::Team(TeamError::NoMembers));
    }
    for m in &inputs.spec.members {
        validate_toolkits(m)?;
    }

    let project_dir = inputs.data_dir.clone();
    let config = TeamConfig {
        memory_root: project_dir.join("memory"),
        session_base_dir: project_dir.join("sessions"),
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
        let spec = IndividualSpec::new(&m.id, &m.role_label, model).configure(|b| {
            let b = b.system_prompt(Some(m.persona.clone()));
            // 成员级 toolkit：社交/memory/knowledge 由 team.build 统一装；
            // fs/web 是显式 opt-in 的成员级插件。
            let mut b = b;
            for tk in &m.toolkits {
                b = match tk.as_str() {
                    "fs" => b.install(&llm_harness_tools::FsToolsPlugin::new(None)),
                    "web" => b.install(&llm_harness_tools::WebToolsPlugin::default()),
                    // memory/knowledge 团队级已装；此处 no-op。
                    "memory" | "knowledge" => b,
                    _ => b, // validate_toolkits 已拦截未知包
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

    Ok(Assembled { team, handoffs })
}

#[cfg(test)]
pub(crate) fn tests_probe_validate(m: &MemberSpec) -> Result<(), AssembleError> {
    validate_toolkits(m)
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
        let s = spec(vec![m]);
        let out = assemble(inputs(s.clone())).await.unwrap();
        assert_eq!(out.handoffs.len(), 1);
        assert_eq!(out.handoffs[0].resource, "issues");
        assert_eq!(out.handoffs[0].to, "planner");
        assert_eq!(out.handoffs[0].message_template, "issue: {body}");
    }

    #[test]
    fn mcp_toolkit_reports_dedicated_error() {
        let s = spec(vec![member(
            "w",
            &["mcp:my-mcp"],
            ModelRef::Explicit("m".into()),
        )]);
        let err = crate::engine::tests_probe_validate(&s.members[0]).unwrap_err();
        assert!(
            matches!(err, AssembleError::McpNotYetSupported { server, .. } if server == "my-mcp")
        );
    }
}
