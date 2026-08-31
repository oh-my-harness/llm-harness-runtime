//! RuleChain —— 规则链，按顺序匹配，首个命中的 rule 的 Decision 生效。

use serde_json::Value;

use crate::rules::rule::{Decision, Rule};

/// 规则链。按顺序匹配，首个命中的 rule 的 Decision 生效。
/// 全部不命中 → 返回 fallback（默认 Deny，fail-secure）。
///
/// 注意排序：`tool_name: "*"` 的通配 Allow 规则会短路后续规则，
/// 必须排在特定 Deny 规则之后，否则 Deny 永不生效。
pub struct RuleChain {
    rules: Vec<Rule>,
    fallback: Decision,
}

impl RuleChain {
    /// 创建一个 builder。
    pub fn builder() -> RuleChainBuilder {
        RuleChainBuilder {
            rules: vec![],
            fallback: Decision::Deny,
        }
    }

    /// 评估给定 tool call，返回决策。
    pub fn evaluate(&self, tool_name: &str, args: &Value) -> Decision {
        self.evaluate_matched(tool_name, args).decision
    }

    /// 评估并附带命中信息（哪条规则命中、谓词描述），供 deny 消息使用。
    pub fn evaluate_matched(&self, tool_name: &str, args: &Value) -> EvaluatedDecision {
        for (index, rule) in self.rules.iter().enumerate() {
            if let Some(decision) = rule.evaluate(tool_name, args) {
                return EvaluatedDecision {
                    decision,
                    matched: Some(MatchedRule {
                        index,
                        tool_name: rule.tool_name.clone(),
                        describe: rule.predicate.describe(),
                    }),
                };
            }
        }
        EvaluatedDecision {
            decision: self.fallback,
            matched: None,
        }
    }
}

/// 命中的规则信息（`evaluate_matched` 返回）。
#[derive(Debug, Clone)]
pub struct MatchedRule {
    /// 规则在链中的下标（0 起）。
    pub index: usize,
    /// 规则的 tool_name 匹配模式（`"*"` 表示通配）。
    pub tool_name: String,
    /// 谓词描述（`Predicate::describe`）。
    pub describe: String,
}

/// `RuleChain::evaluate_matched` 的返回：决策 + 命中信息。
pub struct EvaluatedDecision {
    pub decision: Decision,
    pub matched: Option<MatchedRule>,
}

pub struct RuleChainBuilder {
    rules: Vec<Rule>,
    fallback: Decision,
}

impl RuleChainBuilder {
    /// 追加一条规则。
    pub fn rule(mut self, r: Rule) -> Self {
        self.rules.push(r);
        self
    }

    /// 设置全不命中时的 fallback（默认 Deny）。
    pub fn fallback(mut self, d: Decision) -> Self {
        self.fallback = d;
        self
    }

    /// 构建规则链。
    pub fn build(self) -> RuleChain {
        RuleChain {
            rules: self.rules,
            fallback: self.fallback,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::predicates::Contains;

    #[test]
    fn first_match_wins_over_later() {
        // 两条规则均命中 "measure"，但首条 Allow 生效，第二条 Deny 被短路。
        let chain = RuleChain::builder()
            .rule(Rule {
                tool_name: "measure".into(),
                predicate: Box::new(Contains::new(vec!["measure".into()])),
                on_match: Decision::Allow,
            })
            .rule(Rule {
                tool_name: "measure".into(),
                predicate: Box::new(Contains::new(vec!["measure".into()])),
                on_match: Decision::Deny,
            })
            .fallback(Decision::Deny)
            .build();
        assert_eq!(chain.evaluate("measure", &Value::Null), Decision::Allow);
    }

    #[test]
    fn allow_rule_matches() {
        let chain = RuleChain::builder()
            .rule(Rule {
                tool_name: "measure".into(),
                predicate: Box::new(Contains::new(vec!["measure".into()])), // tool_name 在列表 → true
                on_match: Decision::Allow,
            })
            .fallback(Decision::Deny)
            .build();
        assert_eq!(chain.evaluate("measure", &Value::Null), Decision::Allow);
    }

    #[test]
    fn fallback_deny_when_no_match() {
        let chain = RuleChain::builder().fallback(Decision::Deny).build();
        assert_eq!(chain.evaluate("any", &Value::Null), Decision::Deny);
    }

    #[test]
    fn fallback_allow_when_no_match() {
        let chain = RuleChain::builder().fallback(Decision::Allow).build();
        assert_eq!(chain.evaluate("any", &Value::Null), Decision::Allow);
    }

    #[test]
    fn wildcard_tool_name_matches_any() {
        let chain = RuleChain::builder()
            .rule(Rule {
                tool_name: "*".into(),
                predicate: Box::new(Contains::new(vec!["any_tool".into()])), // tool_name "any_tool" 在列表？
                on_match: Decision::Allow,
            })
            .fallback(Decision::Deny)
            .build();
        // tool_name "*" 匹配任意，但 Contains(["any_tool"]) 测 "foo" → false
        assert_eq!(chain.evaluate("foo", &Value::Null), Decision::Deny);
        // tool_name "any_tool" → Contains 命中
        assert_eq!(chain.evaluate("any_tool", &Value::Null), Decision::Allow);
    }
}
