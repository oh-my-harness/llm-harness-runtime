//! Project pulse: a fact-based situation view.
//!
//! Aggregates what is already observable — per-agent lifecycle status,
//! harness phase, and the social traffic between members (delegate /
//! broadcast / operator messages) — into one snapshot for the panel's
//! activity tab. No task-state-machine modeling: the projection only
//! reports facts the team itself produces.

use llm_harness_agent_team::{AgentId, AgentTeam, SocialContext};

/// One member's current situation.
async fn agent_pulse(team: &AgentTeam, id: &str) -> serde_json::Value {
    let agent_id = AgentId::new(id);
    let status = team
        .agent_status(id)
        .map(|s| format!("{s:?}").to_lowercase())
        .unwrap_or_else(|| "unknown".into());
    // Harness phase mirrors the social status but adds error detail.
    let (phase, error) = team.harness(&agent_id).map_or((None, None), |h| {
        let s = h.state();
        (
            Some(format!("{:?}", s.phase).to_lowercase()),
            s.error_message.clone(),
        )
    });

    // Activity: what a busy agent is actually doing. Facts only, read
    // from the persisted session (the same source the inspector uses):
    // the latest user message is the task it was given; the newest
    // assistant tool_use blocks without a matching tool_result are the
    // tools currently executing.
    let mut activity = serde_json::json!({ "task": null, "tools": [] });
    if status == "busy"
        && let Some(h) = team.harness(&agent_id)
        && let Ok(entries) = h.read_active_path().await
    {
        use llm_harness_agent::session::types::SessionEntryPayload;
        let mut task: Option<String> = None;
        let mut open_tools: Vec<String> = Vec::new();
        for entry in entries.iter().rev() {
            if task.is_none()
                && let SessionEntryPayload::Message(llm_harness_types::AgentMessage::User(u)) =
                    &entry.payload
            {
                let text: String = u
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        llm_harness_types::ContentBlock::Text { text, .. } => Some(text.trim()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                if !text.is_empty() && text.trim() != "Your todolist is empty." {
                    task = Some(text.chars().take(120).collect());
                }
            }
        }
        // Newest assistant tool_use blocks still awaiting a result.
        if let Some(last_assistant) = entries.iter().rev().find_map(|e| match &e.payload {
            SessionEntryPayload::Message(llm_harness_types::AgentMessage::Assistant(a)) => Some(a),
            _ => None,
        }) {
            let answered: std::collections::HashSet<&str> = entries
                .iter()
                .filter_map(|e| match &e.payload {
                    SessionEntryPayload::Message(llm_harness_types::AgentMessage::ToolResult(
                        t,
                    )) => Some(t.tool_use_id.as_str()),
                    _ => None,
                })
                .collect();
            for block in &last_assistant.content {
                if let llm_harness_types::ContentBlock::ToolUse { id, name, .. } = block
                    && !answered.contains(id.as_str())
                {
                    open_tools.push(name.clone());
                }
            }
        }
        activity["task"] = task
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null);
        activity["tools"] = serde_json::json!(open_tools);
    }

    serde_json::json!({
        "id": id,
        "status": status,
        "phase": phase,
        "error": error,
        "activity": activity,
    })
}

/// The pulse snapshot for one project's team.
pub async fn snapshot(team: &AgentTeam, member_ids: &[String]) -> serde_json::Value {
    let mut agents = Vec::new();
    for id in member_ids {
        agents.push(agent_pulse(team, id).await);
    }
    serde_json::json!({ "agents": agents })
}

/// Convenience wrapper over a project's social context: recent social
/// traffic (last N messages between members), for the timeline view.
///
/// Pulls from each member's inbox drain history is not retained by
/// design; instead we snapshot what is *pending* per agent plus what the
/// inspector already streams. The panel combines this with its live WS
/// feed, so nothing is lost.
pub fn pending_traffic(team: &AgentTeam, member_ids: &[String]) -> serde_json::Value {
    let mut pending = Vec::new();
    if let Some(ctx) = team.social_context() {
        let mut ids = member_ids.iter().map(String::as_str).collect::<Vec<_>>();
        ids.push("operator");
        for id in ids {
            let agent = AgentId::new(id);
            for msg in ctx.drain_inbox(&agent) {
                // Draining mutates; returning them keeps state intact.
                pending.push(serde_json::json!({
                    "to": id,
                    "from": msg.from.as_str(),
                    "type": format!("{:?}", msg.message_type).to_lowercase(),
                    "body": msg.body,
                }));
                if let Err(error) = ctx.push_message(msg.clone()) {
                    ctx.return_messages(&agent, vec![msg]);
                    pending.push(serde_json::json!({
                        "to": id,
                        "error": error.to_string(),
                    }));
                }
            }
        }
    }
    serde_json::json!({ "pending": pending })
}

/// Number of live timer tasks owned by the team's social context.
pub fn pending_timers(team: &AgentTeam) -> usize {
    team.social_context()
        .map(SocialContext::pending_timer_count)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::ModelTier;
    use llm_harness_agent_team::TimeoutManager;
    use llm_harness_loop::test_utils::MockLlmClient;
    use std::sync::Arc;
    use std::time::Duration;

    async fn built_team() -> AgentTeam {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let tier = ModelTier {
            strong: "m".into(),
            main: "m".into(),
            cheap: "m".into(),
        };
        let providers: Vec<(String, Arc<dyn llm_harness_loop::LlmClient>)> = vec![(
            "m".into(),
            Arc::new(MockLlmClient::new(vec![])) as Arc<dyn llm_harness_loop::LlmClient>,
        )];
        let repo = std::env::temp_dir().join(format!("ct-pulse-{sequence}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let data_root =
            std::env::temp_dir().join(format!("ct-pulse-data-{sequence}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_root);
        let config = llm_harness_agent_team::TeamConfig {
            memory_root: data_root.join("memory"),
            session_base_dir: data_root.join("sessions"),
            ..Default::default()
        };
        let mut team = AgentTeam::with_config(config);
        for s in crate::roles::RoleSet::standard(&tier).specs {
            team = team.member(s);
        }
        let env: Arc<dyn llm_harness_types::ExecutionEnv> =
            Arc::new(llm_harness_sandbox::OsEnv::new(&repo));
        team.build(env, providers).await.unwrap();
        team
    }

    #[tokio::test]
    async fn snapshot_lists_all_members_with_activity_shape() {
        let mut team = built_team().await;
        let members = built_members();
        let snap = snapshot(&team, &members).await;
        let agents = snap["agents"].as_array().unwrap();
        let ids: Vec<&str> = agents.iter().map(|a| a["id"].as_str().unwrap()).collect();
        assert_eq!(ids.len(), members.len()); // operator excluded
        assert!(
            !ids.contains(&"planner"),
            "V0 coding team must not contain a planner member"
        );
        assert!(
            ids.contains(&"orchestrator"),
            "the LLM orchestrator is a real team member"
        );
        assert!(!ids.contains(&"operator"), "operator has no harness");
        for a in agents {
            assert!(a["activity"]["task"].is_null(), "fresh team: no activity");
            assert_eq!(a["activity"]["tools"], serde_json::json!([]));
        }
        team.shutdown();
    }

    #[tokio::test]
    async fn pending_traffic_drains_without_loss() {
        let mut team = built_team().await;
        team.submit("coder", "fix the bug").unwrap();
        let members = built_members();
        let v = pending_traffic(&team, &members);
        let pending = v["pending"].as_array().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0]["to"], "coder");
        assert!(pending[0]["body"].as_str().unwrap().contains("fix the bug"));
        team.shutdown();
    }

    fn built_members() -> Vec<String> {
        let tier = ModelTier {
            strong: "m".into(),
            main: "m".into(),
            cheap: "m".into(),
        };
        crate::roles::RoleSet::standard(&tier)
            .specs
            .iter()
            .map(|spec| spec.id().as_str().to_string())
            .collect()
    }

    #[tokio::test]
    async fn pending_timer_count_tracks_active_timers() {
        let mut team = built_team().await;
        let ctx = team.social_context().unwrap().clone();
        let timer = TimeoutManager::new(ctx.clone());
        timer
            .set_self_reminder_durably(
                AgentId::new("coder"),
                "check review results",
                Duration::from_millis(10),
                Some("review-check".into()),
            )
            .unwrap();

        assert_eq!(pending_timers(&team), 1);
        timer
            .set_self_reminder_durably(
                AgentId::new("coder"),
                "check again",
                Duration::from_millis(10),
                Some("review-check".into()),
            )
            .unwrap();
        assert_eq!(pending_timers(&team), 1, "same key replaces the timer");

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(pending_timers(&team), 0, "fired timers are removed");
        team.shutdown();
        assert_eq!(pending_timers(&team), 0, "shutdown aborts pending timers");
    }
}
