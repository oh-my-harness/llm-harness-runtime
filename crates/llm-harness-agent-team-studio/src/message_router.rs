//! Message-triggered declarative hand-offs.

use std::sync::Arc;

use llm_harness_agent_team::{AgentId, InboxMessage, MessageObserver, SocialContext};

use crate::team_spec::TeamSpec;

/// A literal-match route from a delivered team message to a target member.
#[derive(Clone, Debug, PartialEq)]
pub struct MessageRoute {
    pub from: Option<String>,
    pub to: Option<String>,
    pub body_contains: String,
    pub target: String,
    pub message_template: String,
}

/// Message-triggered routes declared by the current spec.
pub fn message_routes(spec: &TeamSpec) -> Vec<MessageRoute> {
    spec.members
        .iter()
        .flat_map(|m| {
            m.behaviors.iter().filter_map(|behavior| {
                let crate::team_spec::Behavior::Handoff {
                    on,
                    to,
                    message_template,
                } = behavior
                else {
                    return None;
                };
                let crate::team_spec::Trigger::Message {
                    from,
                    to: trigger_to,
                    body_contains,
                } = on
                else {
                    return None;
                };
                Some(MessageRoute {
                    from: from.clone(),
                    to: trigger_to.clone(),
                    body_contains: body_contains.clone(),
                    target: to.clone(),
                    message_template: message_template.clone(),
                })
            })
        })
        .collect()
}

const DEDUP_CAPACITY: usize = 4096;

/// Executes declarative message hand-offs as targeted public messages.
///
/// Generated messages are sent from the operator and therefore bypass this
/// router; the group projection still records them without waking untargeted
/// peers. A finite declarative route set remains the caller's responsibility.
pub(crate) struct MessageEventRouter {
    routes: Vec<MessageRoute>,
    ctx: SocialContext,
    operator: AgentId,
    seen: parking_lot::Mutex<std::collections::HashSet<(String, usize)>>,
    seen_order: parking_lot::Mutex<std::collections::VecDeque<(String, usize)>>,
}

impl MessageEventRouter {
    pub(crate) fn new(routes: Vec<MessageRoute>, ctx: SocialContext) -> Self {
        Self {
            routes,
            ctx,
            operator: AgentId::new(llm_harness_agent_team::reserved::OPERATOR),
            seen: parking_lot::Mutex::new(std::collections::HashSet::new()),
            seen_order: parking_lot::Mutex::new(std::collections::VecDeque::new()),
        }
    }

    fn mark_seen(&self, key: (String, usize)) {
        if !self.seen.lock().insert(key.clone()) {
            return;
        }
        let mut order = self.seen_order.lock();
        order.push_back(key);
        while order.len() > DEDUP_CAPACITY {
            if let Some(evicted) = order.pop_front() {
                self.seen.lock().remove(&evicted);
            }
        }
    }
}

impl MessageObserver for MessageEventRouter {
    fn on_message(&self, message: &InboxMessage) {
        if message.from == self.operator {
            return;
        }
        let Some(message_id) = message.message_id.as_ref() else {
            return;
        };
        for (route_index, route) in self.routes.iter().enumerate() {
            if let Some(from) = &route.from
                && *from != message.from.as_str()
            {
                continue;
            }
            if let Some(to) = &route.to
                && *to != message.to.as_str()
            {
                continue;
            }
            if !message.body.contains(&route.body_contains) {
                continue;
            }
            let key = (message_id.clone(), route_index);
            if self.seen.lock().contains(&key) {
                continue;
            }
            self.mark_seen(key);
            let rendered = route
                .message_template
                .replace("{from}", message.from.as_str())
                .replace("{to}", message.to.as_str())
                .replace("{body}", &message.body);
            if let Err(error) = self.ctx.send_broadcast_to_with_message_id(
                &self.operator,
                &AgentId::new(&route.target),
                rendered,
                None,
            ) {
                tracing::error!(
                    target = %route.target,
                    source_message_id = %message_id,
                    error = ?error,
                    "message handoff routing failed"
                );
            }
        }
    }
}

/// Register the routes with a built team. Engine calls this after `build` so
/// every target is present before the first source message is observed.
pub(crate) fn install(spec: &crate::team_spec::TeamSpec, team: &llm_harness_agent_team::AgentTeam) {
    let routes = message_routes(spec);
    if routes.is_empty() {
        return;
    }
    let Some(ctx) = team.social_context() else {
        tracing::error!("message handoffs declared but team is not built");
        return;
    };
    let observer_ctx = ctx.clone();
    ctx.add_message_observer(Arc::new(MessageEventRouter::new(routes, observer_ctx)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team_spec::{Behavior, MemberSpec, ModelRef, Trigger};

    fn member(id: &str) -> MemberSpec {
        MemberSpec {
            id: id.into(),
            persona: "p".into(),
            role_label: "r".into(),
            model: ModelRef::Explicit("m".into()),
            toolkits: vec![],
            behaviors: vec![],
            provider_override: None,
            persona_file: None,
        }
    }

    #[test]
    fn extracts_declared_message_routes() {
        let mut m = member("actor");
        m.behaviors = vec![Behavior::Handoff {
            on: Trigger::Message {
                from: Some("verifier".into()),
                to: Some("operator".into()),
                body_contains: "VERDICT: PASS".into(),
            },
            to: "curriculum".into(),
            message_template: "terminal verdict: {body}".into(),
        }];
        let spec = crate::team_spec::TeamSpec {
            id: "t".into(),
            template: None,
            template_baseline: None,
            name: "T".into(),
            repo_path: None,
            sandbox_mode: None,
            members: vec![m, member("verifier"), member("curriculum")],
        };

        assert_eq!(
            message_routes(&spec),
            vec![MessageRoute {
                from: Some("verifier".into()),
                to: Some("operator".into()),
                body_contains: "VERDICT: PASS".into(),
                target: "curriculum".into(),
                message_template: "terminal verdict: {body}".into(),
            }]
        );
    }

    #[tokio::test]
    async fn wakes_target_once_without_recursion() {
        use llm_harness_agent_team::priority;

        let ctx = SocialContext::new(10);
        for id in ["source", "target"] {
            ctx.register_agent(AgentId::new(id), "worker");
        }
        let router_ctx = ctx.clone();
        ctx.add_message_observer(Arc::new(MessageEventRouter::new(
            vec![MessageRoute {
                from: Some("source".into()),
                to: Some("target".into()),
                body_contains: "START".into(),
                target: "target".into(),
                message_template: "wake: {body}".into(),
            }],
            router_ctx,
        )));

        for _ in 0..2 {
            ctx.send_direct_with_message_id(
                &AgentId::new("source"),
                &AgentId::new("target"),
                "STATUS: done. START",
                priority::NORMAL,
                None,
                None,
                Some("logical-message".into()),
            )
            .unwrap();
        }
        let messages = ctx.drain_inbox(&AgentId::new("target"));
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.body == "wake: STATUS: done. START")
                .count(),
            1
        );
    }
}
