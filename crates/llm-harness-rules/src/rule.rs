//! Rule 与 Decision。

use crate::predicate::Predicate;

/// 规则命中后的决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

/// 单条规则。
pub struct Rule {
    /// 工具名匹配；`"*"` = 所有工具（保留名，无法匹配字面名为 `*` 的工具）。
    /// 通配 Allow 规则会短路后续规则，排序时务必排在特定 Deny 规则之后。
    pub tool_name: String,
    /// 谓词。需要从 args 取值的 predicate（`NumberRangeField`/`RegexField`）内部持 `arg_path`。
    pub predicate: Box<dyn Predicate>,
    /// 命中时的决策。
    pub on_match: Decision,
}

impl Rule {
    /// 判断此规则是否对给定 tool call 命中。
    /// 命中 = tool_name 匹配（`"*"` 通配）+ predicate.test 返回 true。
    fn matches(&self, tool_name: &str, args: &serde_json::Value) -> bool {
        let name_ok = self.tool_name == "*" || self.tool_name == tool_name;
        if !name_ok {
            return false;
        }
        self.predicate.test(tool_name, args)
    }

    /// 返回命中时的决策（未命中返回 None）。
    pub fn evaluate(&self, tool_name: &str, args: &serde_json::Value) -> Option<Decision> {
        if self.matches(tool_name, args) {
            Some(self.on_match)
        } else {
            None
        }
    }
}

impl Rule {
    /// 创建一条规则。
    pub fn new(
        tool_name: impl Into<String>,
        predicate: Box<dyn Predicate>,
        on_match: Decision,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            predicate,
            on_match,
        }
    }
}
