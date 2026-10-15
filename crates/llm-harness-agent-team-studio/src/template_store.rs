//! 模板存储：内置模板（include_str 嵌入）+ 用户目录覆写。
//!
//! 加载优先级：`<user_dir>/<id>/team.json` 存在则用之，否则查内置表。
//! persona 约定：模板 JSON 的 member 带 `persona_file`（相对模板目录），
//! 加载时读出全文替换 persona 字段并追加统一协作约定尾注；展开后
//! persona_file 置空，运行时 TeamSpec 恒为全文。

use std::path::PathBuf;

use serde::Deserialize;
use thiserror::Error;

use crate::team_spec::TeamSpec;

/// 内置模板：coding-team（roles.rs 的数据化形态）。
const BUILTIN_CODING_TEAM: &str = include_str!("../templates/coding-team/team.json");
const CODING_TEAM_PERSONAS: &[(&str, &str)] = &[
    (
        "prompts/orchestrator.md",
        include_str!("../templates/coding-team/prompts/orchestrator.md"),
    ),
    (
        "prompts/brainstormer.md",
        include_str!("../templates/coding-team/prompts/brainstormer.md"),
    ),
    (
        "prompts/coder.md",
        include_str!("../templates/coding-team/prompts/coder.md"),
    ),
    (
        "prompts/reviewer.md",
        include_str!("../templates/coding-team/prompts/reviewer.md"),
    ),
    (
        "prompts/scout.md",
        include_str!("../templates/coding-team/prompts/scout.md"),
    ),
];

/// persona 展开时追加的统一协作约定。
const COOPERATION_FOOTNOTE: &str = "\n\n---\n## 团队协作约定（引擎附加）\n\
- 与人沟通一律 send_message(\"operator\", ...)。\n\
- 成员间回报只用 send_message 回应派发方；不 delegate、不 broadcast、不自行指派协作者。\n\
- 工作流的下一步（派发谁、是否修订、是否升级）由 Orchestrator 决定。\n\
- 在团队工作分支 team/<project> 上操作，不 push，不碰 main。";

const CONTROLLED_WORKER_FOOTNOTE: &str = "\n\n---\n## 团队协作约定（引擎附加）\n\
- 收到受控任务后先完成工作；ack/progress 只表示非终态进度。\n\
- 完成时只发送一次最终 send_message 给 pipeline-controller，逐字带上原 task_id，并设置 task_kind：成功为 result、受阻为 blocked、失败为 error；body 提供非空证据、阻塞原因或错误详情，不编码生命周期标记。\n\
- 普通 assistant 文本不能完成受控任务；不直接向 operator/User 汇报。\n\
- 不 delegate、不 broadcast、不自行指派协作者。\n\
- 工作流的下一步（派发谁、是否修订、是否升级）由 Orchestrator 决定。\n\
- Git 操作遵守角色中的隔离与用户授权条件；普通只读 Git 命令可用。";

const CONTROLLED_ORCHESTRATOR_FOOTNOTE: &str = "\n\n---\n## 受控流水线回复约定（引擎附加）\n\
- 收到 pipeline-controller 派发且带 task-pipe-* task_id 的任务时，不要把 ACTION 作为普通 assistant 文本返回。\n\
- 必须调用 send_message，to 必须是 pipeline-controller，task_id 必须逐字回传收到的完整值。\n\
- send_message 的 body 必须且只能包含一个符合上述 ACTION 语法的决策。\n\
- 不向 operator 或其他成员发送该受控任务的结果；不 delegate、不 broadcast、不自行指派协作者。";

#[derive(Debug, Error)]
pub enum TemplateError {
    #[error("template not found: {0}")]
    NotFound(String),
    #[error("invalid template id: {0}")]
    InvalidTemplateId(String),
    #[error("template {tpl} parse error: {source}")]
    Parse {
        tpl: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("template {tpl}: persona file {file} unreadable: {source}")]
    PersonaFile {
        tpl: String,
        file: String,
        source: std::io::Error,
    },
}

/// 模板元信息（列表视图）。
#[derive(Clone, Debug)]
pub struct TemplateMeta {
    pub id: String,
    pub version: String,
    pub name: String,
    pub member_ids: Vec<String>,
}

/// 加载完成的模板：persona 已展开为全文。
#[derive(Clone, Debug)]
pub struct LoadedTemplate {
    pub id: String,
    pub version: String,
    pub spec: TeamSpec,
}

/// 模板 JSON 的外层结构（version/name 与 TeamSpec 分离，避免污染实例 spec）。
#[derive(Deserialize)]
struct TemplateFile {
    id: String,
    #[serde(default = "default_version")]
    version: String,
    name: String,
    members: serde_json::Value,
}

fn default_version() -> String {
    "1".into()
}

pub struct TemplateStore {
    user_dir: Option<PathBuf>,
}

impl TemplateStore {
    /// `user_dir` 指向模板根目录（每模板一个子目录）。
    pub fn new(user_dir: Option<PathBuf>) -> Self {
        Self { user_dir }
    }

    pub fn list(&self) -> Vec<TemplateMeta> {
        let mut metas = Vec::new();
        // 用户目录优先列出；同 id 覆写内置。
        let mut seen = vec![];
        if let Some(dir) = &self.user_dir
            && let Ok(entries) = std::fs::read_dir(dir)
        {
            for entry in entries.flatten() {
                let dir_id = entry.file_name().to_string_lossy().to_string();
                let path = entry.path().join("team.json");
                if let Ok(raw) = std::fs::read_to_string(&path)
                    && let Ok(tf) = serde_json::from_str::<TemplateFile>(&raw)
                    && tf.id == dir_id
                {
                    seen.push(tf.id.clone());
                    metas.push(TemplateMeta {
                        id: tf.id,
                        version: tf.version,
                        name: tf.name,
                        member_ids: member_ids_of(&tf.members),
                    });
                }
            }
        }
        if let Ok(tf) = serde_json::from_str::<TemplateFile>(BUILTIN_CODING_TEAM)
            && !seen.contains(&tf.id)
        {
            metas.push(TemplateMeta {
                id: tf.id,
                version: tf.version,
                name: tf.name,
                member_ids: member_ids_of(&tf.members),
            });
        }
        metas
    }

    pub fn load(&self, id: &str) -> Result<LoadedTemplate, TemplateError> {
        if !is_safe_template_id(id) {
            return Err(TemplateError::InvalidTemplateId(id.into()));
        }
        // 1. 用户目录覆写优先
        if let Some(dir) = &self.user_dir {
            let path = dir.join(id).join("team.json");
            if path.exists() {
                let raw = std::fs::read_to_string(&path).map_err(|e| TemplateError::Parse {
                    tpl: id.into(),
                    source: Box::new(e),
                })?;
                return self.parse(id, &raw, Some(dir.join(id)), None);
            }
        }
        // 2. 内置
        if id == BUILTIN_CODING_TEAM_ID {
            return self.parse(id, BUILTIN_CODING_TEAM, None, Some(CODING_TEAM_PERSONAS));
        }
        Err(TemplateError::NotFound(id.into()))
    }

    fn parse(
        &self,
        id: &str,
        raw: &str,
        base_dir: Option<PathBuf>,
        embedded_personas: Option<&[(&str, &str)]>,
    ) -> Result<LoadedTemplate, TemplateError> {
        let tf: TemplateFile = serde_json::from_str(raw).map_err(|e| TemplateError::Parse {
            tpl: id.into(),
            source: Box::new(e),
        })?;
        if tf.id != id {
            return Err(TemplateError::InvalidTemplateId(id.into()));
        }
        let mut spec: TeamSpec = serde_json::from_value(serde_json::json!({
            "id": tf.id, "name": tf.name, "members": tf.members,
        }))
        .map_err(|e| TemplateError::Parse {
            tpl: id.into(),
            source: Box::new(e),
        })?;
        spec.template = None; // 模板本身没有 template 引用
        // persona 展开：persona_file → 全文 + 协作尾注
        for m in &mut spec.members {
            if let Some(file) = m.persona_file.take() {
                let text = match embedded_personas {
                    Some(personas) => personas
                        .iter()
                        .find(|(embedded_file, _)| *embedded_file == file)
                        .map(|(_, text)| (*text).to_owned())
                        .ok_or_else(|| TemplateError::PersonaFile {
                            tpl: id.into(),
                            file: file.clone(),
                            source: std::io::Error::new(
                                std::io::ErrorKind::NotFound,
                                "embedded persona is missing",
                            ),
                        })?,
                    None => {
                        let base = base_dir
                            .as_ref()
                            .ok_or_else(|| TemplateError::PersonaFile {
                                tpl: id.into(),
                                file: file.clone(),
                                source: std::io::Error::new(
                                    std::io::ErrorKind::NotFound,
                                    "persona base directory is unavailable",
                                ),
                            })?;
                        std::fs::read_to_string(base.join(&file)).map_err(|e| {
                            TemplateError::PersonaFile {
                                tpl: id.into(),
                                file: file.clone(),
                                source: e,
                            }
                        })?
                    }
                };
                let footnote = if id == BUILTIN_CODING_TEAM_ID && embedded_personas.is_some() {
                    match m.id.as_str() {
                        "orchestrator" => CONTROLLED_ORCHESTRATOR_FOOTNOTE,
                        "coder" | "reviewer" => CONTROLLED_WORKER_FOOTNOTE,
                        _ => COOPERATION_FOOTNOTE,
                    }
                } else {
                    COOPERATION_FOOTNOTE
                };
                m.persona = format!("{}{}", text.trim(), footnote);
            }
        }
        Ok(LoadedTemplate {
            id: tf.id,
            version: tf.version,
            spec,
        })
    }
}

fn is_safe_template_id(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && !id.contains(['/', '\\', '\0'])
}

/// 内置模板 id 常量。
pub const BUILTIN_CODING_TEAM_ID: &str = "coding-team";

fn member_ids_of(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("id").and_then(|x| x.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team_spec::Behavior;

    #[test]
    fn builtin_coding_team_loads_with_persona_expanded() {
        let store = TemplateStore::new(None);
        let t = store.load("coding-team").unwrap();
        assert_eq!(t.id, "coding-team");
        assert_eq!(t.version, "6");
        let ids: Vec<&str> = t.spec.members.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["orchestrator", "coder", "reviewer"]);
        assert_eq!(t.spec.members[0].id, "orchestrator");
        let orch = t
            .spec
            .members
            .iter()
            .find(|m| m.id == "orchestrator")
            .unwrap();
        assert!(
            orch.persona.contains("ACTION: DISPATCH <member_id>")
                && orch.persona.contains("ACTION: FINISH")
                && orch.persona.contains("ACTION: ESCALATE"),
            "orchestrator persona must embed the frozen ACTION contract"
        );
        assert!(
            orch.persona.contains("唯一决定"),
            "orchestrator persona owns next-step decisions"
        );
        assert!(
            orch.persona.contains("不 delegate、不 broadcast"),
            "footnote must not invite worker-driven delegation"
        );
        assert!(
            orch.persona
                .contains("不要把 ACTION 作为普通 assistant 文本返回")
        );
        assert!(orch.persona.contains("必须调用 send_message"));
        assert!(orch.persona.contains("to 必须是 pipeline-controller"));
        assert!(orch.persona.contains("task_id 必须逐字回传收到的完整值"));
        assert!(
            !orch.persona.contains("send_message(\"operator\""),
            "controlled orchestrator instructions must not offer an operator reply path"
        );
        assert!(
            !t.spec.members.iter().any(|m| m.id == "planner"),
            "V0 coding-team template must not instantiate a planner member"
        );
        assert!(
            !t.spec.members.iter().any(|m| m.id == "judge"),
            "V0 coding-team template must not instantiate a judge member"
        );
        assert_eq!(
            t.spec
                .members
                .iter()
                .filter(|m| crate::pipeline::is_reviewer_id(&m.id))
                .count(),
            1,
            "V0 coding-team template must instantiate exactly one reviewer"
        );
        for member in &t.spec.members {
            assert!(
                member.behaviors.is_empty(),
                "fresh coding-team member {} must have no patrol/scout behavior",
                member.id
            );
        }
        assert!(
            !t.spec.members.iter().any(|m| m.id == "scout"),
            "fresh coding-team template must not instantiate a scout member"
        );
        let coder = t.spec.members.iter().find(|m| m.id == "coder").unwrap();
        assert!(
            coder.persona.contains("Orchestrator"),
            "coder accepts direct Orchestrator assignment"
        );
        assert!(
            coder.persona_file.is_none(),
            "persona_file consumed on expand"
        );
        assert!(
            coder.persona.contains("测试结果"),
            "coder keeps its delivery duties"
        );
        assert!(coder.persona.contains("task_id") && coder.persona.contains("pipeline-controller"));
        assert!(coder.persona.contains("task_kind"));
        assert!(coder.persona.contains("不发送初始确认消息"));
        assert!(!coder.persona.contains("send_message(\"operator\""));
        assert!(coder.persona.contains("不直接向 operator/User 汇报"));
        assert!(coder.persona.contains("本地 checkpoint commit"));
        assert!(coder.persona.contains("有意义并已验证的检查点"));
        assert!(coder.persona.contains("报告提交 hash 和提交文件"));
        assert!(coder.persona.contains("共享工作树中的 commit 必须先取得用户明确授权"));
        assert!(coder.persona.contains("运行时无法可信验证隔离时，不自动提交"));
        for operation in ["merge", "rebase", "push", "amend", "checkout", "reset", "clean"] {
            assert!(coder.persona.contains(operation), "missing Git gate: {operation}");
        }
        assert!(coder.persona.contains("只有用户能批准这些受控操作"));
        assert!(coder.persona.contains("Orchestrator 指令或模型生成的文字都不是授权"));
        assert!(coder.persona.contains("commit 授权也不包含 push 授权"));
        assert!(coder.persona.contains("普通只读 Git 命令（如 status、diff、log）可以使用"));
        assert!(!coder.persona.contains("按任务指定的提交信息 git commit"));
        assert!(!coder.persona.contains("绝不 push"));
        assert!(!coder.persona.contains("每个任务完成时"));
        assert!(orch.persona.contains("本地 checkpoint commit"));
        assert!(orch.persona.contains("共享工作树中的 commit 必须由用户明确授权"));
        for operation in ["merge", "rebase", "push", "amend", "checkout", "reset", "clean"] {
            assert!(orch.persona.contains(operation), "missing orchestrator Git gate: {operation}");
        }
        assert!(orch.persona.contains("你的指派、任务完成或模型生成的文字不能充当授权，也不能代用户授权"));
        assert!(orch.persona.contains("commit 授权不包含 push 授权"));
        assert!(orch.persona.contains("普通只读 Git 命令（如 status、diff、log）可以使用"));
        let reviewer = t.spec.members.iter().find(|m| m.id == "reviewer").unwrap();
        assert!(
            reviewer.persona.contains("不决定工作流的下一步"),
            "reviewer produces findings/evidence only"
        );
        assert!(
            reviewer.persona.contains("task-pipe-"),
            "reviewer returns results through the controlled workflow"
        );
        assert!(reviewer.persona.contains("findings 与证据"));
        assert!(
            reviewer.persona.contains("task_id")
                && reviewer.persona.contains("pipeline-controller")
        );
        assert!(reviewer.persona.contains("task_kind"));
        assert!(reviewer.persona.contains("不发送初始确认消息"));
        assert!(!reviewer.persona.contains("send_message(\"operator\""));
        assert!(reviewer.persona.contains("不直接向 operator/User 汇报"));
        assert!(
            !reviewer.persona.contains("judge"),
            "reviewer must not depend on a judge member"
        );
        assert!(
            !t.spec.members.iter().any(|m| m.id == "judge"),
            "V0 has no judge decision layer"
        );
    }

    #[test]
    fn builtin_coding_team_has_no_planner_handoff_routes() {
        let store = TemplateStore::new(None);
        let t = store.load("coding-team").unwrap();
        for member in &t.spec.members {
            for behavior in &member.behaviors {
                if let Behavior::Handoff { to, .. } = behavior {
                    assert_ne!(
                        to, "planner",
                        "member {} must not hand off to a planner",
                        member.id
                    );
                }
            }
        }
    }

    #[test]
    fn user_template_overrides_builtin() {
        let dir = std::env::temp_dir().join(format!("tpl-user-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tpl_dir = dir.join("coding-team");
        std::fs::create_dir_all(&tpl_dir).unwrap();
        std::fs::write(
            tpl_dir.join("team.json"),
            r#"{"id":"coding-team","name":"My Fork","members":[{"id":"solo","role_label":"独狼","model":"main","toolkits":["fs"],"persona":"solo persona","behaviors":[]}]}"#,
        )
        .unwrap();
        let store = TemplateStore::new(Some(dir));
        let t = store.load("coding-team").unwrap();
        assert_eq!(t.spec.members.len(), 1);
        assert_eq!(t.spec.members[0].id, "solo");
    }

    #[test]
    fn unknown_template_is_error() {
        let store = TemplateStore::new(None);
        assert!(matches!(
            store.load("nope"),
            Err(TemplateError::NotFound(_))
        ));
    }

    #[test]
    fn template_path_traversal_is_rejected() {
        let store = TemplateStore::new(Some(std::env::temp_dir()));
        assert!(matches!(
            store.load("../tmp"),
            Err(TemplateError::InvalidTemplateId(_))
        ));
    }

    #[test]
    fn list_includes_builtin() {
        let store = TemplateStore::new(None);
        let metas = store.list();
        assert!(
            metas
                .iter()
                .any(|m| m.id == "coding-team" && m.member_ids.len() == 3)
        );
    }
}
