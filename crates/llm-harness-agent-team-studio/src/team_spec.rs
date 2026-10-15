//! TeamSpec：声明式团队配置的数据模型。
//!
//! TeamSpec 是 studio 引擎的唯一输入：模板展开后与实例覆写后都收敛到这个
//! 强类型结构，装配器只认识它。序列化约定：
//! - `model` 用 untagged 字符串区分三档引用与显式模型名（命中
//!   strong/main/cheap → Tier，其余 → Explicit）
//! - `persona_file` 是模板 JSON 的中间态键，加载展开后不进序列化输出

use serde::{Deserialize, Serialize};

use crate::roles::ModelTier;

/// 团队规格：模板引用 + 实例成员列表。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TeamSpec {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<TemplateRef>,
    /// Snapshot of the template members used when this team was created;
    /// this is the baseline that decides which members may be upgraded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_baseline: Option<TemplateBaseline>,
    pub name: String,
    /// 工作仓库；None = 数据目录兜底（非 coding 类团队）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_path: Option<String>,
    /// Execution-env confinement actually applied at team creation:
    /// `bwrap` (workspace-scoped sandbox) or `trusted-host` (explicit operator
    /// grant). This field is output, not policy input. Recorded for evaluation comparability;
    /// `None` = created by an older build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_mode: Option<String>,
    pub members: Vec<MemberSpec>,
}

impl TeamSpec {
    /// repo_path 的 PathBuf 视图（bin 恢复团队用）。
    pub fn repo_hint(&self) -> Option<std::path::PathBuf> {
        self.repo_path.as_ref().map(std::path::PathBuf::from)
    }
}

/// 实例来源的模板引用。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TemplateRef {
    pub template_id: String,
    pub version: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TemplateBaseline {
    pub version: String,
    pub members: Vec<MemberSpec>,
}

/// 成员规格：覆写 = 全字段替换。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MemberSpec {
    pub id: String,
    /// 协作约定全文（模板加载时已展开 persona_file）；模板 JSON 中间态可为空。
    #[serde(default)]
    pub persona: String,
    pub role_label: String,
    pub model: ModelRef,
    #[serde(default)]
    pub toolkits: Vec<String>,
    #[serde(default)]
    pub behaviors: Vec<Behavior>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_override: Option<ProviderOverride>,
    /// 模板 JSON 中间态键：persona 从此文件展开，展开后即 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona_file: Option<String>,
}

/// 模型引用：三档（引用全局 settings）或显式模型 id。
/// 字符串在序列化时保持简洁；反序列化时仅把保留档位名识别为 Tier，
/// 其余名称必须保留 Explicit 语义，避免持久化往返后回退到未知档位。
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum ModelRef {
    Tier(String),
    Explicit(String),
}

impl<'de> Deserialize<'de> for ModelRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if matches!(value.as_str(), "strong" | "main" | "cheap") {
            Ok(ModelRef::Tier(value))
        } else {
            Ok(ModelRef::Explicit(value))
        }
    }
}

impl ModelRef {
    /// 解析为真实模型 id；未知三档名回退 main 并告警。
    pub fn resolve(&self, models: &ModelTier) -> String {
        match self {
            ModelRef::Tier(t) if t == "strong" => models.strong.clone(),
            ModelRef::Tier(t) if t == "main" => models.main.clone(),
            ModelRef::Tier(t) if t == "cheap" => models.cheap.clone(),
            ModelRef::Tier(t) => {
                tracing::warn!(tier = %t, "unknown model tier, falling back to main");
                models.main.clone()
            }
            ModelRef::Explicit(m) => m.clone(),
        }
    }
}

/// 引擎解释的声明式行为。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Behavior {
    /// 周期巡检；task 为空 = 引擎生成默认巡检文本。
    Patrol {
        interval_secs: u64,
        #[serde(default)]
        task: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        issues_dir: Option<String>,
    },
    /// 资源确认后的交接路由。
    Handoff {
        on: Trigger,
        to: String,
        message_template: String,
    },
}

/// Handoff 触发条件。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    ResourceConfirmed { resource: String },
}

/// 成员级 provider 覆写；None 字段继承全局。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct ProviderOverride {
    /// 明文 key 只存 0600 secrets 文件；spec 序列化永不携带（C1）。
    #[serde(skip)]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl TeamSpec {
    pub fn member_ids(&self) -> Vec<&str> {
        self.members.iter().map(|m| m.id.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tier() -> ModelTier {
        ModelTier {
            strong: "g-5".into(),
            main: "g-4".into(),
            cheap: "g-4-air".into(),
        }
    }

    fn spec(members: Vec<MemberSpec>) -> TeamSpec {
        TeamSpec {
            id: "t".into(),
            template: None,
            template_baseline: None,
            name: "T".into(),
            repo_path: None,
            sandbox_mode: None,
            members,
        }
    }

    fn member(id: &str) -> MemberSpec {
        MemberSpec {
            id: id.into(),
            persona: "p".into(),
            role_label: "r".into(),
            model: ModelRef::Explicit("explicit-model".into()),
            toolkits: vec![],
            behaviors: vec![],
            provider_override: None,
            persona_file: None,
        }
    }

    #[test]
    fn spec_json_roundtrip() {
        let s = spec(vec![MemberSpec {
            id: "planner".into(),
            persona: "你是 planner".into(),
            role_label: "规划者".into(),
            model: ModelRef::Tier("strong".into()),
            toolkits: vec!["fs".into()],
            behaviors: vec![Behavior::Handoff {
                on: Trigger::ResourceConfirmed {
                    resource: "issues".into(),
                },
                to: "planner".into(),
                message_template: "issue: {body}".into(),
            }],
            provider_override: Some(ProviderOverride {
                api_key: None,
                base_url: Some("https://x.io".into()),
                model: None,
            }),
            persona_file: None,
        }]);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<TeamSpec>(&json).unwrap(), s);
    }

    #[test]
    fn template_baseline_omitted_when_none() {
        let s = spec(vec![member("a")]);
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("template_baseline"));
        let mut s = s;
        s.template_baseline = Some(TemplateBaseline {
            version: "1".into(),
            members: s.members.clone(),
        });
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("template_baseline"));
        assert_eq!(serde_json::from_str::<TeamSpec>(&json).unwrap(), s);
    }

    #[test]
    fn model_ref_resolution() {
        let t = tier();
        assert_eq!(ModelRef::Tier("strong".into()).resolve(&t), "g-5");
        assert_eq!(ModelRef::Tier("main".into()).resolve(&t), "g-4");
        assert_eq!(ModelRef::Tier("cheap".into()).resolve(&t), "g-4-air");
        assert_eq!(
            ModelRef::Explicit("claude-x".into()).resolve(&t),
            "claude-x"
        );
        // 未知档位回退 main
        assert_eq!(ModelRef::Tier("bogus".into()).resolve(&t), "g-4");
    }

    #[test]
    fn member_ids_lists_all() {
        let s = spec(vec![member("a"), member("b")]);
        assert_eq!(s.member_ids(), vec!["a", "b"]);
    }

    #[test]
    fn persona_file_omitted_when_none() {
        let m = member("a");
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("persona_file"));
        // Some 时保留（模板 JSON 的中间态键需要落盘）
        let mut m = m;
        m.persona_file = Some("prompts/a.md".into());
        assert!(serde_json::to_string(&m).unwrap().contains("persona_file"));
    }
}
