#![cfg(feature = "live-llm")]

//! Real-model AgentTeam tests.
//!
//! These tests are ignored by default because they require an external
//! OpenAI-compatible GLM gateway and can take minutes to complete.

use std::env;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use llm_harness_agent::session::types::{SessionEntry, SessionEntryPayload};
use llm_harness_agent_team::{
    AgentStatus, AgentTeam, IndividualSpec, TeamConfig, format_transcript,
};
use llm_harness_loop::{LlmClient, openai_compatible};
use llm_harness_types::{AgentMessage, ContentBlock, ExecutionEnv};

const GLM_MODEL: &str = "GLM-5.3-Flash";
const TEST_TIMEOUT: Duration = Duration::from_secs(300);

fn required_env(name: &str) -> String {
    env::var(name).unwrap_or_else(|error| panic!("{name} must be set: {error}"))
}

fn member(id: &str, expertise: &str, system_prompt: &str) -> IndividualSpec {
    IndividualSpec::new(id, expertise, GLM_MODEL).configure(|builder| {
        builder
            .system_prompt(Some(system_prompt.to_string()))
            .max_tokens(4096)
            .temperature(Some(0.0))
    })
}

async fn member_diagnostics(team: &AgentTeam, member: &str) -> String {
    let status = team
        .agent_status(member)
        .map(|status| format!("{status:?}"))
        .unwrap_or_else(|| "unregistered".to_string());
    let harness_error = team
        .harness(&llm_harness_agent_team::AgentId::new(member))
        .map(|harness| {
            harness
                .state()
                .error_message
                .unwrap_or_else(|| "none".to_string())
        })
        .unwrap_or_else(|| "harness unavailable".to_string());
    let transcript = match team.harness(&llm_harness_agent_team::AgentId::new(member)) {
        Some(harness) => match harness.read_all_entries().await {
            Ok(entries) => format_transcript(&entries),
            Err(error) => format!("transcript read failed: {error}"),
        },
        None => String::new(),
    };
    format!(
        "\n--- {member} ---\nstatus={status}\nharness_error={harness_error}\ntranscript:\n{transcript}"
    )
}

async fn tool_calls(team: &AgentTeam, member: &str) -> Vec<(String, serde_json::Value)> {
    let Some(harness) = team.harness(&llm_harness_agent_team::AgentId::new(member)) else {
        panic!("{member} harness must be observable after build");
    };
    let entries = harness
        .read_all_entries()
        .await
        .unwrap_or_else(|error| panic!("{member} transcript should be readable: {error}"));
    assistant_tool_calls(&entries)
}

fn assistant_tool_calls(entries: &[SessionEntry]) -> Vec<(String, serde_json::Value)> {
    let mut calls = Vec::new();
    for entry in entries {
        let SessionEntryPayload::Message(AgentMessage::Assistant(message)) = &entry.payload else {
            continue;
        };
        for block in &message.content {
            if let ContentBlock::ToolUse { name, input, .. } = block {
                calls.push((name.clone(), input.clone()));
            }
        }
    }
    calls
}

fn require_single_tool_call(
    calls: &[(String, serde_json::Value)],
    member: &str,
    tool_name: &str,
    target: &str,
) {
    let matching: Vec<_> = calls
        .iter()
        .filter(|(name, input)| {
            name == tool_name && input.get("to").and_then(serde_json::Value::as_str) == Some(target)
        })
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "{member} must call {tool_name} to {target} exactly once; calls={calls:?}"
    );
    let input = &matching[0].1;
    assert_eq!(
        input.get("to").and_then(serde_json::Value::as_str),
        Some(target),
        "{member} must target {target} with {tool_name}; input={input}"
    );
}

#[tokio::test]
#[ignore = "requires a live GLM-5.3-Flash OpenAI-compatible gateway"]
async fn glm_5_3_flash_agent_team_delegates_coding_review_to_operator() {
    let api_key = required_env("OPENAI_API_KEY");
    let base_url = required_env("OPENAI_API_BASE");
    let model = required_env("OPENAI_MODEL");
    assert_eq!(model, GLM_MODEL, "this E2E must run GLM-5.3-Flash");

    let client: Arc<dyn LlmClient> = Arc::new(openai_compatible(api_key, base_url));
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let config = TeamConfig {
        memory_root: workspace.path().join("memory"),
        session_base_dir: workspace.path().join("sessions"),
        ..TeamConfig::default()
    };
    let env: Arc<dyn ExecutionEnv> = Arc::new(llm_harness_sandbox::OsEnv::new(
        workspace.path().to_path_buf(),
    ));

    let mut team = AgentTeam::with_config(config)
        .member(member(
            "planner",
            "decompose coding work and delegate it to the right peer",
            "Route the workflow to coder with delegate_task exactly once. The task argument must \
             tell coder to implement checksum, then delegate the exact implementation and checks \
             to reviewer. Do not broadcast or message the operator.",
        ))
        .member(member(
            "coder",
            "write small, correct Rust functions",
            "When delegated an implementation, delegate the complete result to reviewer with \
             delegate_task. Do not message the operator.",
        ))
        .member(member(
            "reviewer",
            "verify Rust implementations and report the exact result",
            "When delegated a review, verify the requested cases and send the required result to \
             the operator with send_message.",
        ));
    team.build(env, vec![(GLM_MODEL.to_string(), client)])
        .await
        .expect("team build should succeed");

    let task = "Delegate to coder: implement a Rust function \
                 checksum(parts: &[u8]) -> u8 that XOR-folds all bytes, then delegate \
                 to reviewer: verify checksum(&[]) == 0 and checksum(&[1, 2, 3]) == 0. \
                 Reviewer must send the operator exactly: \
                 APPROVED | empty=0 | sample=0";
    team.submit("planner", task)
        .expect("submit should accept a registered member");

    let deadline = tokio::time::Instant::now() + TEST_TIMEOUT;
    let mut operator_messages = Vec::new();
    let reviewer_body = {
        let mut stream = std::pin::pin!(team.receive().expect("built team yields a stream"));
        loop {
            let message = match tokio::time::timeout_at(deadline, stream.next()).await {
                Ok(Some(message)) => message,
                Ok(None) => panic!("operator receive stream must not end"),
                Err(_) => {
                    let diagnostics = futures::future::join_all(
                        ["planner", "coder", "reviewer"]
                            .into_iter()
                            .map(|id| member_diagnostics(&team, id)),
                    )
                    .await
                    .join("");
                    panic!(
                        "timed out waiting for reviewer; messages={operator_messages:?}; diagnostics={diagnostics}"
                    );
                }
            };
            println!("{}: {}", message.from, message.body);
            if message.from.as_str() == "reviewer" {
                assert!(
                    message.body.trim() == "APPROVED | empty=0 | sample=0",
                    "reviewer must report the verified checksum result: {}",
                    message.body
                );
                break Some(message.body);
            }
            operator_messages.push((message.from.to_string(), message.body));
        }
    };
    assert!(
        reviewer_body.is_some(),
        "reviewer must send the final message"
    );

    let planner_calls = tool_calls(&team, "planner").await;
    let coder_calls = tool_calls(&team, "coder").await;
    let reviewer_calls = tool_calls(&team, "reviewer").await;
    require_single_tool_call(&planner_calls, "planner", "delegate_task", "coder");
    require_single_tool_call(&coder_calls, "coder", "delegate_task", "reviewer");
    require_single_tool_call(&reviewer_calls, "reviewer", "send_message", "operator");
    let reviewer_message = reviewer_calls
        .iter()
        .find(|(name, input)| {
            *name == "send_message"
                && input.get("to").and_then(serde_json::Value::as_str) == Some("operator")
        })
        .map(|(_, input)| input.clone())
        .expect("reviewer operator send_message call is asserted above");
    assert!(
        reviewer_message
            .get("body")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|body| body.trim() == "APPROVED | empty=0 | sample=0"),
        "reviewer operator payload must equal the verified result: {reviewer_message}"
    );

    team.shutdown();
    for member in ["planner", "coder", "reviewer"] {
        assert_eq!(
            team.agent_status(member),
            Some(AgentStatus::Stopped),
            "{member} must stop cleanly"
        );
    }
}
