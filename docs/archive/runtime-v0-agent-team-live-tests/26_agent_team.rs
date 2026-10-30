//! 26 — Agent Team: a self-driving multi-agent team with a live LLM.
//!
//! Demonstrates:
//! - `AgentTeam` / `IndividualSpec`: declarative team composition + `build`
//! - Peer-to-peer collaboration: `delegate_task` routes work between members
//! - Operator interaction: `submit` tasks in, `receive` results out
//! - Reflection loop: agents persist reusable lessons via `memory_write`
//!   into a SQLite-backed store (survives process restarts) and dedupe via
//!   `knowledge_search`
//!
//! Run:
//! ```text
//! source .env
//! cargo run -p llm-harness-live-tests --example 26_agent_team
//! ```
//!
//! Expected duration: 60–120 s total (three scenarios; single LLM round
//! trips are 10–30 s each).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use llm_harness_agent_team::{AgentTeam, IndividualSpec, TeamConfig};
use llm_harness_live_tests::{LiveProvider, LiveProviderEntry};
use llm_harness_types::ExecutionEnv;

/// Per-scenario wall-clock budget. Real LLM latency for one scenario is
/// 15–30 s; 90 s is a 3–6x margin that also bounds failure detection.
const SCENARIO_TURN: Duration = Duration::from_secs(90);

/// Compose a team whose members all run on `entry`'s provider/model.
///
/// Returns the built team plus a guard directory holding the team's
/// persistent state (memory SQLite db + session base dir).
async fn make_team(
    entry: &LiveProviderEntry,
    member_specs: &[(&str, &str)],
    reflection_interval: usize,
) -> (AgentTeam, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("team tempdir");
    let config = TeamConfig {
        memory_root: dir.path().join("memory"),
        session_base_dir: dir.path().join("sessions"),
        ..TeamConfig::default()
    };
    let mut team = AgentTeam::with_config(config);
    for (id, expertise) in member_specs {
        let mut spec = IndividualSpec::new(*id, *expertise, entry.model.as_str());
        if reflection_interval > 0 && *id == "a1" {
            spec = spec.reflection_interval(reflection_interval);
        }
        team = team.member(spec);
    }
    let env: Arc<dyn ExecutionEnv> = Arc::new(llm_harness_sandbox::OsEnv::new("/tmp"));
    let providers = vec![(entry.model.clone(), entry.client.clone())];
    team.build(env, providers)
        .await
        .expect("team build should succeed");
    (team, dir)
}

/// Submit a task and wait for an operator-visible reply.
///
/// `expect_from` filters whose reply counts: scenario 2 passes "a2" so the
/// intermediate "I've delegated it" confirmation from a1 doesn't end the
/// wait early — the operator sees every message as it arrives, but only a
/// reply from the named sender proves the work actually flowed through the
/// delegation.
async fn submit_and_collect(
    team: &mut AgentTeam,
    target: &str,
    task: &str,
    expect_from: Option<&str>,
) {
    println!("Operator → {target}: \"{task}\"\n");
    team.submit(target, task).expect("submit should succeed");

    let mut stream = std::pin::pin!(team.receive().expect("built team yields a stream"));
    let deadline = tokio::time::Instant::now() + SCENARIO_TURN;
    loop {
        let msg = match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(msg)) => msg,
            Ok(None) => panic!("receive stream must not end"),
            Err(_) => {
                // agent_status turns a bare timeout into a diagnosis:
                // Busy → the agent is still working (wait longer);
                // Idle → the run finished but sent nothing to the operator
                // (the task text likely lacks a "send to operator"
                // instruction); None → the id is not observable at all.
                let status = team.agent_status(target);
                let what = match status {
                    Some(llm_harness_agent_team::AgentStatus::Busy) => "is still Busy (working)",
                    Some(llm_harness_agent_team::AgentStatus::Idle) => {
                        "is Idle but sent nothing to the operator"
                    }
                    Some(_) => "has an unusual status",
                    None => "is not observable",
                };
                panic!("timed out waiting for the team to report back: {target} {what}");
            }
        };
        println!("Operator ← {}: \"{}\"\n", msg.from, msg.body);
        match expect_from {
            // Any reply satisfies the round trip.
            None => return,
            // Wait specifically for a named sender.
            Some(sender) if msg.from.as_str() == sender => return,
            _ => continue,
        }
    }
}

#[tokio::main]
async fn main() {
    println!("=== 26: Agent Team (delegate → reply → operator, reflection) ===\n");

    let provider = LiveProvider::from_env();
    let entry: &LiveProviderEntry = provider
        .all_providers()
        .into_iter()
        .next()
        .expect("set OPENAI_API_KEY (or ANTHROPIC_API_KEY) to run this example");

    println!("Provider: {} | Model: {}\n", entry.name, entry.model);

    // ── Scenario 1: peer-to-peer message flow ────────────────────────────
    println!("--- Scenario 1: operator → a1 → operator round trip ---\n");
    {
        let (mut team, _dir) = make_team(entry, &[("a1", "research"), ("a2", "writing")], 0).await;
        submit_and_collect(
            &mut team,
            "a1",
            "What is the capital of France? Answer in one sentence, then \
             send the answer to the operator with send_message.",
            None,
        )
        .await;
        team.shutdown();
        println!("(team shut down)\n");
    }

    println!("--- Scenario 2: operator → a1 delegates → a2 → operator ---\n");
    {
        let (mut team, _dir) = make_team(
            entry,
            &[("a1", "coordinator"), ("a2", "poet"), ("a3", "reviewer")],
            0,
        )
        .await;
        submit_and_collect(
            &mut team,
            "a1",
            "Delegate to a2: write a two-line haiku about databases. a2 \
             must send the haiku directly to the operator with send_message.",
            Some("a2"),
        )
        .await;
        team.shutdown();
        println!("(team shut down)\n");
    }

    // ── Scenario 3: reflection persists a lesson, then dedupe finds it ───
    //
    // NOTE: reflection_interval is 1 here. A run triggered solely by a
    // ReflectionTrigger message does not count toward the next trigger
    // (the runner skips reflection-only runs — see AgentRunner in
    // llm-harness-agent-team), so the task run fires exactly one
    // reflection and the loop settles instead of recursing. The old
    // guidance ("use 2, or interval=1 loops forever") described the
    // pre-fix self-recursion bug and no longer applies.
    println!("--- Scenario 3: reflection writes a lesson; knowledge_search dedupes ---\n");
    {
        let (mut team, dir) =
            make_team(entry, &[("a1", "rust engineer"), ("a2", "historian")], 1).await;
        submit_and_collect(
            &mut team,
            "a1",
            "Explain in two sentences why Rust's borrow checker prevents \
             data races. Finish with a line starting with \"Learned:\" \
             containing a one-sentence reusable takeaway, then send the \
             answer to the operator with send_message.",
            None,
        )
        .await;

        // The reflection trigger fires after runs complete and pushes a
        // ReflectionTrigger message into a1's inbox; the follow-up run
        // writes the lesson via memory_write. Poll the persistent store
        // with a hard attempt cap — the reflection machinery is
        // asynchronous to the operator reply we already received.
        let memory_db: PathBuf = dir.path().join("memory").join("memory.sqlite3");
        println!("Waiting for the reflection loop to persist a lesson…\n");
        let (source, _store) = llm_harness_agent_team::TeamSqliteMemorySource::open(&memory_db)
            .expect("team memory db should exist after build");
        let mut found = false;
        for attempt in 1..=30 {
            // Must match the runner's injected context: scope "agent-team",
            // principal ("agent", agent_id) — see team.rs build loop.
            let request = llm_harness_types::RunRequest::from_text("t").with_extension(
                llm_harness_knowledge::KnowledgeAccessContext::new(
                    llm_harness_knowledge::KnowledgeScope::new("agent-team"),
                    llm_harness_knowledge::PrincipalRef::new("a1", "agent"),
                ),
            );
            let run = llm_harness_types::RunContext::new(request);
            let ctx = llm_harness_knowledge::KnowledgeRequestContext::from_run(&run)
                .expect("access context extension present");
            let page = llm_harness_knowledge::KnowledgeSource::search(
                &*source,
                ctx,
                llm_harness_knowledge::SourceSearchRequest {
                    query: "borrow checker data races".into(),
                    filters: vec![],
                    limit: 5,
                    cursor: None,
                },
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("team memory search should work");
            if !page.hits.is_empty() {
                found = true;
                for hit in &page.hits {
                    println!(
                        "Memory hit (attempt {attempt}): {} (item {})",
                        hit.title.as_deref().unwrap_or_default(),
                        hit.reference.item_id
                    );
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(2_000)).await;
        }
        if found {
            println!("\n(reflection loop closed: lesson persisted and searchable)\n");
        } else {
            println!(
                "\n(no lesson persisted within the polling budget — the model \
                 may have judged the transcript not worth persisting; that is \
                 a legitimate abstention)\n"
            );
        }
        team.shutdown();
        println!("(team shut down)\n");
    }

    println!("=== 26: done ===");
}
