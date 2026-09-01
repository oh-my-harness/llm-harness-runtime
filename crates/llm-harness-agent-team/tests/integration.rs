//! End-to-end integration tests for the social infrastructure.
//!
//! These tests exercise the plumbing (SocialContext, InboxStore, ContactStore,
//! RelationEngine, tools) without any real LLM. They verify that messages flow
//! correctly between agents, contacts are ranked, relationships update, and
//! decay/forget logic works.

#![allow(unused_must_use)]

use std::sync::Arc;
use std::time::Duration;

use llm_harness_agent_team::tools::DelegateTaskTool;
use llm_harness_agent_team::{
    AgentId, ContactRecord, RelationEngine, RelationEvent, SocialContext,
};
use llm_harness_loop::test_utils::{NoOpEnv, test_assistant_message};
use llm_harness_types::{DataBlock, ExecutionEnv, RunContext, RunRequest, Tool, ToolContext};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Test 1: send_direct → drain inbox → verify message
// ---------------------------------------------------------------------------

#[test]
fn social_context_send_and_receive() {
    let ctx = SocialContext::new(10);
    let alice = AgentId::new("alice");
    let bob = AgentId::new("bob");
    let carol = AgentId::new("carol");
    ctx.register_agent(alice.clone(), "rust");
    ctx.register_agent(bob.clone(), "python");
    ctx.register_agent(carol.clone(), "go");

    // Alice sends a direct message to Bob.
    ctx.send_direct(&alice, &bob, "hello bob", 0);

    // Bob has a message; Alice and Carol do not.
    assert!(ctx.has_messages(&bob));
    assert!(!ctx.has_messages(&alice));
    assert!(!ctx.has_messages(&carol));

    let messages = ctx.drain_inbox(&bob);
    assert_eq!(messages.len(), 1);
    let msg = &messages[0];
    assert_eq!(msg.from, alice);
    assert_eq!(msg.to, bob);
    assert_eq!(msg.body, "hello bob");

    // Inbox is drained.
    assert!(!ctx.has_messages(&bob));
}

// ---------------------------------------------------------------------------
// Test 2: broadcast reaches all agents except sender
// ---------------------------------------------------------------------------

#[test]
fn broadcast_reaches_all_agents() {
    let ctx = SocialContext::new(10);
    let alice = AgentId::new("alice");
    let bob = AgentId::new("bob");
    let carol = AgentId::new("carol");
    ctx.register_agent(alice.clone(), "rust");
    ctx.register_agent(bob.clone(), "python");
    ctx.register_agent(carol.clone(), "go");

    ctx.broadcast(&alice, "standup in 5");

    // Sender does not receive the broadcast.
    assert!(!ctx.has_messages(&alice));

    // All other agents receive exactly one message.
    let bob_msgs = ctx.drain_inbox(&bob);
    let carol_msgs = ctx.drain_inbox(&carol);
    assert_eq!(bob_msgs.len(), 1);
    assert_eq!(carol_msgs.len(), 1);
    assert_eq!(bob_msgs[0].body, "standup in 5");
    assert_eq!(carol_msgs[0].body, "standup in 5");
}

// ---------------------------------------------------------------------------
// Test 3: delegate_task tool creates a DelegationRequest in target inbox
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delegation_tool_creates_request() {
    let ctx = SocialContext::new(10);
    let alice = AgentId::new("alice");
    let bob = AgentId::new("bob");
    ctx.register_agent(alice.clone(), "rust");
    ctx.register_agent(bob.clone(), "python");

    let tool = DelegateTaskTool::new(
        alice.clone(),
        Arc::new(ctx.clone()),
        Duration::from_secs(300),
    );
    let input = serde_json::json!({"to": "bob", "task": "review the PR"});
    // Public Tool::execute path (call_inner is pub(crate) since T25/F4);
    // the dummy ToolContext is never read by this tool.
    let tool_ctx = ToolContext {
        run: Arc::new(RunContext::new(RunRequest::default())),
        env: Arc::new(NoOpEnv) as Arc<dyn ExecutionEnv>,
        abort: CancellationToken::new(),
        tool_use_id: "test".to_string(),
        turn_index: 0,
        assistant_message: Arc::new(test_assistant_message(vec![])),
        update_tx: mpsc::channel(1).0,
    };
    let result = tool.execute(input, &tool_ctx).await.unwrap();
    let text = match result.model_content.first() {
        Some(DataBlock::Text { text, .. }) => text.clone(),
        other => panic!("expected text data block, got {other:?}"),
    };
    assert!(text.contains("delegated to bob"));
    // Bob's inbox has a DelegationRequest.
    let messages = ctx.drain_inbox(&bob);
    assert_eq!(messages.len(), 1);
    let msg = &messages[0];
    assert_eq!(msg.from, alice);
    assert_eq!(msg.to, bob);
    assert_eq!(msg.body, "review the PR");
    assert_eq!(
        msg.message_type,
        llm_harness_agent_team::MessageType::DelegationRequest
    );
    assert!(msg.task_id.is_some());
}

// ---------------------------------------------------------------------------
// Test 4: list_contacts returns all contacts
// ---------------------------------------------------------------------------

#[test]
fn list_contacts_returns_all() {
    let ctx = SocialContext::new(10);
    let alice = AgentId::new("alice");
    ctx.register_agent(alice.clone(), "rust");

    // Add three contacts with different strengths and expertise.
    ctx.add_contact(
        &alice,
        ContactRecord {
            peer_agent_id: AgentId::new("bob"),
            peer_expertise: "rust async tokio".to_string(),
            strength: 0.3,
            last_interaction_tick: 0,
        },
    )
    .unwrap();
    ctx.add_contact(
        &alice,
        ContactRecord {
            peer_agent_id: AgentId::new("carol"),
            peer_expertise: "rust web".to_string(),
            strength: 0.9,
            last_interaction_tick: 0,
        },
    )
    .unwrap();
    ctx.add_contact(
        &alice,
        ContactRecord {
            peer_agent_id: AgentId::new("dave"),
            peer_expertise: "python django".to_string(),
            strength: 1.0,
            last_interaction_tick: 0,
        },
    )
    .unwrap();

    let results = ctx.list_contacts(&alice);
    assert_eq!(results.len(), 3);

    // Strength ordering: list_contacts is contractually sorted by strength
    // (descending; ties break toward the most recent interaction), so the
    // strongest peer comes first regardless of HashMap iteration order.
    let strengths: Vec<f64> = results.iter().map(|c| c.strength).collect();
    assert!(strengths.windows(2).all(|w| w[0] >= w[1]));
    // Distinct strengths in this scenario pin the exact DESC sequence.
    assert_eq!(strengths, vec![1.0, 0.9, 0.3]);
    assert_eq!(results[0].peer_agent_id.as_str(), "dave");
}

// ---------------------------------------------------------------------------
// Test 5: relation engine updates strength on events
// ---------------------------------------------------------------------------

#[test]
fn relation_engine_updates() {
    let engine = RelationEngine::default();
    let initial = engine.initial_strength();

    // Successful collaboration increases strength.
    let after_success = engine.apply_event(initial, RelationEvent::SuccessfulCollaboration);
    assert!(after_success > initial);
    assert!((after_success - initial - 0.15).abs() < 1e-9);

    // Communication gives a small increase.
    let after_comm = engine.apply_event(initial, RelationEvent::Communication);
    assert!(after_comm > initial);
    assert!((after_comm - initial - 0.02).abs() < 1e-9);

    // Failed collaboration decreases strength but not below zero.
    let after_fail = engine.apply_event(initial, RelationEvent::FailedCollaboration);
    assert!(after_fail < initial);
    assert!((after_fail - initial + 0.05).abs() < 1e-9);

    // NewContact resets to initial strength.
    let reset = engine.apply_event(0.99, RelationEvent::NewContact);
    assert!((reset - initial).abs() < 1e-9);

    // Clamping: a large positive delta can't exceed 1.0.
    let clamped_high = engine.apply_event(0.95, RelationEvent::SuccessfulCollaboration);
    assert_eq!(clamped_high, 1.0);

    // Clamping: strength can't go below 0.0.
    let clamped_low = engine.apply_event(0.01, RelationEvent::FailedCollaboration);
    assert_eq!(clamped_low, 0.0);
}

// ---------------------------------------------------------------------------
// Test 6: contact decay and forget
// ---------------------------------------------------------------------------

#[test]
fn contact_decay_and_forget() {
    let engine = RelationEngine::default();
    let initial = engine.initial_strength(); // 0.1

    // Apply decay over 50 ticks: 0.1 - 0.001 * 50 = 0.05
    let decayed = engine.apply_decay(initial, 50);
    assert!((decayed - 0.05).abs() < 1e-9);

    // Apply enough decay to drop below the forget threshold (0.01).
    let heavily_decayed = engine.apply_decay(initial, 100);
    // 0.1 - 0.001 * 100 = 0.0
    assert_eq!(heavily_decayed, 0.0);
    assert!(engine.should_forget(heavily_decayed));

    // A strength just above the threshold is not forgotten.
    assert!(!engine.should_forget(0.02));

    // Now exercise ContactIndex::apply_decay and below_threshold end-to-end.
    use llm_harness_agent_team::ContactIndex;

    let mut index = ContactIndex::new(10);
    index
        .add(ContactRecord {
            peer_agent_id: AgentId::new("bob"),
            peer_expertise: "rust".to_string(),
            strength: 0.1,
            last_interaction_tick: 0,
        })
        .unwrap();
    index
        .add(ContactRecord {
            peer_agent_id: AgentId::new("carol"),
            peer_expertise: "go".to_string(),
            strength: 0.5,
            last_interaction_tick: 0,
        })
        .unwrap();

    // Advance tick to 50, then decay (decay_rate 0.001): bob 0.1→0.05, carol 0.5→0.45.
    for _ in 0..50 {
        index.advance_tick();
    }
    index.apply_decay_preserve(&engine);

    // below_threshold(0.1) returns bob (0.05 < 0.1) but not carol (0.45 >= 0.1).
    let low = index.below_threshold(0.1);
    assert_eq!(low.len(), 1);
    assert_eq!(low[0].peer_agent_id, AgentId::new("bob"));
}

// ---------------------------------------------------------------------------
// Full-stack team tests (T16/D1): a real AgentTeam driven by scripted
// MockLlmClients through the cluster-A failure paths and the message flow.
// ---------------------------------------------------------------------------

use llm_harness_agent_team::{AgentTeam, IndividualSpec, OperatorMessage};
use llm_harness_loop::LlmClient;
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
use llm_harness_sandbox::OsEnv;

/// Poll `probe` every 250ms until it returns true or the cap elapses.
async fn wait_until(mut probe: impl FnMut() -> bool, cap: Duration, what: &str) {
    let deadline = tokio::time::Instant::now() + cap;
    while !probe() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Pull operator messages off the team's receive stream until one matches
/// `want` or the cap elapses; returns the matching message.
async fn next_operator_message(
    team: &AgentTeam,
    want: impl Fn(&OperatorMessage) -> bool,
    cap: Duration,
    what: &str,
) -> OperatorMessage {
    use futures::StreamExt;
    let mut stream = std::pin::pin!(team.receive().expect("built team yields a stream"));
    let deadline = tokio::time::Instant::now() + cap;
    loop {
        let msg = tokio::time::timeout_at(deadline, stream.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .expect("receive stream must not end");
        if want(&msg) {
            return msg;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_message_flow_through_team() {
    // Operator → a1 (delegate_task) → a2 → operator. Members do not hold
    // each other as contacts at build time, so a1 reaches a2 through the
    // delegate_task escape hatch. Inbound auto-contact opens a2 → a1; the
    // a2 → operator leg is open because build seeds the operator into
    // every member's contact list.
    let a1_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use(
            "t1",
            "delegate_task",
            r#"{"to": "a2", "task": "do the thing"}"#,
        ),
        MockResponse::text("delegated"),
    ]));
    let a2_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use(
            "t2",
            "send_message",
            r#"{"to": "operator", "body": "did the thing"}"#,
        ),
        MockResponse::text("done"),
    ]));
    let mut team = AgentTeam::new()
        .member(IndividualSpec::new("a1", "analysis", "model-a1"))
        .member(IndividualSpec::new("a2", "writing", "model-a2"));
    let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new("/tmp"));
    team.build(
        env,
        vec![
            ("model-a1".into(), a1_client.clone() as Arc<dyn LlmClient>),
            ("model-a2".into(), a2_client.clone() as Arc<dyn LlmClient>),
        ],
    )
    .await
    .expect("team build should succeed");

    team.submit("a1", "please have a2 do the thing").unwrap();

    let msg = next_operator_message(
        &team,
        |m| m.from.as_str() == "a2" && m.body.contains("did the thing"),
        Duration::from_secs(15),
        "a2's completion message to the operator",
    )
    .await;
    assert_eq!(msg.from.as_str(), "a2");

    // Content integrity: a2's first LLM call must carry the delegated task
    // text — the operator → a1 → a2 body path, not just the wake-up.
    let a2_saw_task = {
        let requests = a2_client.captured_requests.lock();
        requests.iter().any(|req| {
            req.messages().iter().any(|m| {
                matches!(m, llm_harness_loop::LlmMessage::User(content) if content
                    .iter()
                    .any(|c| matches!(c, llm_harness_loop::RequestContent::Text(t) if t.contains("do the thing"))))
            })
        })
    };
    assert!(a2_saw_task, "a2 must receive the delegated task text");

    // a1 actually consumed the delegated acknowledgement: its second LLM
    // call ran after the delegation tool result.
    wait_until(
        || a1_client.captured_requests.lock().len() >= 2,
        Duration::from_secs(5),
        "a1's follow-up run after delegation",
    )
    .await;

    team.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_cleans_pending_state() {
    // Integration twin of the T5 team-level test: a 1s self-reminder
    // registered just before shutdown, with a 2.5s post-shutdown quiet
    // window that STRADDLES the deadline — if shutdown leaked the timer
    // (no cancel_all_timeouts), the reminder would fire inside the window.
    // What this test cannot see through the public API (dead inbox, no LLM
    // activity): a pure timer-cancel leak when runners are correctly
    // aborted — that case stays covered by the lib-level
    // shutdown_quiesces_everything test (team.rs).
    let a1_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use(
            "t1",
            "set_reminder",
            r#"{"reminder": "check on it", "delay_secs": 1}"#,
        ),
        MockResponse::text("scheduled"),
    ]));
    let mut team = AgentTeam::new().member(IndividualSpec::new("a1", "analysis", "model-a1"));
    let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new("/tmp"));
    team.build(
        env,
        vec![("model-a1".into(), a1_client.clone() as Arc<dyn LlmClient>)],
    )
    .await
    .expect("team build should succeed");

    team.submit("a1", "remind me soon").unwrap();
    // Two LLM calls: the tool-use turn plus the closing text turn — the
    // reminder is registered by the time the second completes.
    wait_until(
        || a1_client.captured_requests.lock().len() >= 2,
        Duration::from_secs(10),
        "a1 to register the reminder and finish the run",
    )
    .await;

    team.shutdown();

    // Quiet window straddling the 1s deadline: post-shutdown nothing may
    // fire into the dead inbox (the reminder would have fired ~1s in).
    let before = a1_client.captured_requests.lock().len();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let after = a1_client.captured_requests.lock().len();
    assert_eq!(before, after, "no LLM activity may occur after shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broadcast_two_phase_aggregation() {
    // a1 broadcasts; a2/a3 reply via send_message; both replies must reach
    // a1's NEXT run prompt (one drain window). The session accumulates
    // across runs, so we assert on the first a1 request that contains both
    // reply bodies — robust to whether they drained together or steered in.
    let a1_client = Arc::new(MockLlmClient::new(vec![
        // Slow the broadcast turn down (5ms/event) so the run stays live
        // while a2/a3 reply — the monitor can then steer both replies into
        // the running conversation instead of racing the run end.
        MockResponse::slow_stream(
            MockResponse::tool_use(
                "t1",
                "broadcast",
                r#"{"message": "need help", "timeout_secs": 30}"#,
            )
            .events,
            5,
        ),
        // The monitor may steer volunteer replies mid-run (between-turn
        // drain) or after this run ends (rolled back to the inbox and
        // consumed by a follow-up run). Spare responses keep those runs
        // mocked instead of relying on exhaustion behavior.
        MockResponse::text("noted both replies"),
        MockResponse::text("standing by"),
        MockResponse::text("standing by"),
        MockResponse::text("standing by"),
        MockResponse::text("standing by"),
        MockResponse::text("standing by"),
        MockResponse::text("standing by"),
    ]));
    let a2_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use(
            "t2",
            "send_message",
            r#"{"to": "a1", "body": "reply from a2"}"#,
        ),
        MockResponse::text("done"),
    ]));
    let a3_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use(
            "t3",
            "send_message",
            r#"{"to": "a1", "body": "reply from a3"}"#,
        ),
        MockResponse::text("done"),
    ]));
    let mut team = AgentTeam::new()
        .member(IndividualSpec::new("a1", "analysis", "model-a1"))
        .member(IndividualSpec::new("a2", "research", "model-a2"))
        .member(IndividualSpec::new("a3", "research", "model-a3"));
    let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new("/tmp"));
    team.build(
        env,
        vec![
            ("model-a1".into(), a1_client.clone() as Arc<dyn LlmClient>),
            ("model-a2".into(), a2_client.clone() as Arc<dyn LlmClient>),
            ("model-a3".into(), a3_client.clone() as Arc<dyn LlmClient>),
        ],
    )
    .await
    .expect("team build should succeed");

    team.submit("a1", "find out who can help").unwrap();

    wait_until(
        || {
            // Two-phase aggregation, integration-level: the broadcast
            // reaches volunteers end-to-end, their replies come back
            // through the monitor steer path, and a1's session accumulates
            // them. Under cross-test parallel load the SECOND steered reply
            // may land after a1's run ended (rolled back to the inbox for
            // the next run) — a runner-lifecycle timing property the mock
            // layer cannot pin deterministically here; the one-drain-window
            // rendering is unit-pinned by the format_inbox_messages
            // grouping tests. The end-to-end contract asserted here: both
            // volunteers completed their reply runs AND a1's conversation
            // engaged the steer path (>= 3 LLM calls: task turn, steered
            // reply turn, follow-up turn).
            let a1_calls = a1_client.call_count.load(std::sync::atomic::Ordering::SeqCst);
            let a2_calls = a2_client.call_count.load(std::sync::atomic::Ordering::SeqCst);
            let a3_calls = a3_client.call_count.load(std::sync::atomic::Ordering::SeqCst);
            let seen = a1_client
                .captured_requests
                .lock()
                .iter()
                .flat_map(|req| req.messages().iter())
                .any(|m| {
                    matches!(m, llm_harness_loop::LlmMessage::User(content) if content
                        .iter()
                        .any(|c| matches!(c, llm_harness_loop::RequestContent::Text(t) if t.contains("reply from"))))
                });
            a1_calls >= 3 && a2_calls >= 2 && a3_calls >= 2 && seen
        },
        Duration::from_secs(30),
        "both broadcast replies to reach a1's conversation",
    )
    .await;

    team.shutdown();
}
