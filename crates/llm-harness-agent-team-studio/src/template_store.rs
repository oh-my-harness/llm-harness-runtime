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

/// persona 展开时追加的统一协作约定。
const COOPERATION_FOOTNOTE: &str = "\n\n---\n## 团队协作约定（引擎附加）\n\
- 与人沟通一律 send_message(\"operator\", ...)。\n\
- 与成员协作用 delegate_task / send_message / broadcast。\n\
- 在团队工作分支 team/<project> 上操作，不 push，不碰 main。";

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
                return self.parse(id, &raw, Some(dir.join(id)));
            }
        }
        // 2. 内置
        if id == BUILTIN_CODING_TEAM_ID {
            return self.parse(id, BUILTIN_CODING_TEAM, None);
        }
        Err(TemplateError::NotFound(id.into()))
    }

    fn parse(
        &self,
        id: &str,
        raw: &str,
        base_dir: Option<PathBuf>,
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
        let base = base_dir.unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("templates")
                .join(id)
        });
        for m in &mut spec.members {
            if let Some(file) = m.persona_file.take() {
                let text = std::fs::read_to_string(base.join(&file)).map_err(|e| {
                    TemplateError::PersonaFile {
                        tpl: id.into(),
                        file,
                        source: e,
                    }
                })?;
                m.persona = format!("{}{}", text.trim(), COOPERATION_FOOTNOTE);
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
        assert_eq!(t.version, "2");
        assert_eq!(t.spec.members.len(), 11);
        let planner = t.spec.members.iter().find(|m| m.id == "planner").unwrap();
        assert!(
            planner.persona.contains("delegate_task"),
            "persona expanded from md"
        );
        assert!(
            planner.persona.contains("send_message"),
            "cooperation footnote appended"
        );
        assert!(
            planner.persona_file.is_none(),
            "persona_file consumed on expand"
        );
        let scout = t.spec.members.iter().find(|m| m.id == "scout").unwrap();
        assert!(matches!(scout.behaviors[0], Behavior::Patrol { .. }));
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
                .any(|m| m.id == "coding-team" && m.member_ids.len() == 11)
        );
    }
}
