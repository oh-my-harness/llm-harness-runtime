//! End-to-end integration tests: mock LLM driving real teams through the
//! full delivery chain.
//!
//! Each test registers projects through the real `TeamManager`, feeds
//! agents pre-recorded tool-call responses, and asserts on observable
//! outcomes (inbox contents, issue files, operator stream).

use std::sync::Arc;

use llm_harness_agent_team::AgentId;
use llm_harness_agent_team_studio::issue::IssueStatus;
use llm_harness_agent_team_studio::roles::ModelTier;
use llm_harness_agent_team_studio::team_manager::TeamManager;
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
use tokio::sync::RwLock;

/// Serialize tests: install_mock swaps the process-wide provider factory,
/// so tests sharing it must not interleave register + agent turns.
static FACTORY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn unique_dir(prefix: &str) -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{n}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn make_repo() -> std::path::PathBuf {
    let dir = unique_dir("ct-e2e-repo");
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    dir
}

fn tier() -> ModelTier {
    ModelTier {
        strong: "m".into(),
        main: "m".into(),
        cheap: "m".into(),
    }
}

/// Install the process-wide provider factory with one mock client (model
/// "m"); the same pre-recorded sequence is shared by every agent, so each
/// test must only rely on the *first* agent-driven turn that follows its
/// submit.
fn install_mock(sequence: Vec<MockResponse>) {
    let client: Arc<dyn llm_harness_loop::LlmClient> = Arc::new(MockLlmClient::new(sequence));
    llm_harness_agent_team_studio::env_cfg::install_provider_factory(Arc::new(move || {
        vec![("m".into(), client.clone())]
    }));
}

/// Install model-isolated mock clients so an agent chain can run without
/// sharing one response queue across concurrent runner tasks.
fn install_mock_chain(clients: Vec<(&str, Vec<MockResponse>)>) {
    let providers: Vec<(String, Arc<dyn llm_harness_loop::LlmClient>)> = clients
        .into_iter()
        .map(|(model, sequence)| {
            let client: Arc<dyn llm_harness_loop::LlmClient> =
                Arc::new(MockLlmClient::new(sequence));
            (model.to_owned(), client)
        })
        .collect();
    llm_harness_agent_team_studio::env_cfg::install_provider_factory(Arc::new(move || {
        providers.clone()
    }));
}

fn settings() -> llm_harness_agent_team_studio::settings::Settings {
    llm_harness_agent_team_studio::settings::Settings {
        models: tier(),
        scout_interval_secs: 0,
        base_url: String::new(),
    }
}

/// Standard coding-team-shaped spec with all models pinned to "m" and a
/// repo. Scout's patrol interval comes from the test (0 = no scout).
fn spec_with_scout(
    id: &str,
    repo: &std::path::Path,
    data_root: &std::path::Path,
    scout_interval: u64,
) -> llm_harness_agent_team_studio::team_spec::TeamSpec {
    let _ = repo;
    use llm_harness_agent_team_studio::team_spec::{Behavior, MemberSpec, ModelRef, TeamSpec};
    let mut members: Vec<MemberSpec> = ["planner", "judge"]
        .iter()
        .map(|mid| MemberSpec {
            id: mid.to_string(),
            persona: format!("{mid} persona"),
            role_label: mid.to_string(),
            model: ModelRef::Explicit("m".into()),
            toolkits: vec!["fs".into()],
            behaviors: vec![Behavior::Handoff {
                on: llm_harness_agent_team_studio::team_spec::Trigger::ResourceConfirmed {
                    resource: "issues".into(),
                },
                to: "planner".into(),
                message_template: "以下 issue 已由操作员确认，按其内容拆解任务：\n\nid: {id}\ntitle: {title}\n\n{body}".into(),
            }],
            provider_override: None,
            persona_file: None,
        })
        .collect();
    if scout_interval > 0 {
        members.push(MemberSpec {
            id: "scout".into(),
            persona: "scout persona".into(),
            role_label: "巡检员".into(),
            model: ModelRef::Explicit("m".into()),
            toolkits: vec!["fs".into()],
            behaviors: vec![Behavior::Patrol {
                interval_secs: scout_interval,
                task: String::new(),
                issues_dir: Some(
                    data_root
                        .join("teams")
                        .join(id)
                        .join("issues")
                        .to_string_lossy()
                        .to_string(),
                ),
            }],
            provider_override: None,
            persona_file: None,
        });
    }
    TeamSpec {
        id: id.into(),
        template: None,
        template_baseline: None,
        name: id.into(),
        repo_path: Some(repo.to_string_lossy().to_string()),
        members,
    }
}

async fn drain_planner(pm: &RwLock<TeamManager>, project: &str) -> Vec<String> {
    pm.read()
        .await
        .get(project)
        .unwrap()
        .team
        .social_context()
        .unwrap()
        .drain_inbox(&AgentId::new("planner"))
        .into_iter()
        .map(|m| m.body)
        .collect()
}

/// The minimal coding-team relay: planner delegates implementation, coder
/// reports back through inbound auto-contact, planner delegates review, and
/// judge reports to the operator. This exercises the social rules that the
/// template personas depend on (`delegate_task`, task-id replies, and the
/// operator gate) as one asynchronous chain.
#[tokio::test]
async fn coding_team_relay_completes_through_operator() {
    let _guard = FACTORY_LOCK.lock().await;
    install_mock_chain(vec![
        (
            "planner",
            vec![
                MockResponse::tool_use(
                    "p1",
                    "delegate_task",
                    r#"{"to": "coder", "task": "task-1: implement the change", "timeout_secs": 60}"#,
                ),
                MockResponse::tool_use(
                    "p2",
                    "delegate_task",
                    r#"{"to": "judge", "task": "review task-1 output", "timeout_secs": 60}"#,
                ),
            ],
        ),
        (
            "coder",
            vec![MockResponse::tool_use_end_turn(
                "c1",
                "send_message",
                r#"{"to": "planner", "task_id": "task-planner-0", "body": "task-1 done"}"#,
            )],
        ),
        (
            "judge",
            vec![MockResponse::tool_use_end_turn(
                "j1",
                "send_message",
                r##"{"to": "operator", "body": "# Code Review 报告\n\n## 总体结论: PASS\n\n无 P0/P1 finding。"}"##,
            )],
        ),
    ]);

    let repo = make_repo();
    let data_root = unique_dir("ct-e2e-relay");
    let spec = llm_harness_agent_team_studio::team_spec::TeamSpec {
        id: "relay".into(),
        template: None,
        template_baseline: None,
        name: "relay".into(),
        repo_path: Some(repo.to_string_lossy().to_string()),
        members: ["planner", "coder", "judge"]
            .iter()
            .map(|id| llm_harness_agent_team_studio::team_spec::MemberSpec {
                id: (*id).into(),
                persona: format!("{id} persona"),
                role_label: id.to_string(),
                model: llm_harness_agent_team_studio::team_spec::ModelRef::Explicit((*id).into()),
                toolkits: vec![],
                behaviors: vec![],
                provider_override: None,
                persona_file: None,
            })
            .collect(),
    };
    let mut pm = TeamManager::new(&data_root);
    pm.create(spec, Some(repo), &settings(), "").await.unwrap();

    pm.get("relay")
        .unwrap()
        .team
        .submit("planner", "deliver task-1 and review it")
        .unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let operator_messages = {
            let ctx = pm
                .get("relay")
                .unwrap()
                .team
                .social_context()
                .unwrap()
                .clone();
            ctx.drain_inbox(&AgentId::new("operator"))
        };
        if operator_messages
            .iter()
            .any(|message| message.body.contains("总体结论: PASS"))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "relay did not reach the operator within 10s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    pm.shutdown("relay").await.unwrap();
}

/// The coding-team review loop depends on reviewer→judge without an
/// intervening delegation. This deterministic chain covers a full
/// NEEDS_CHANGES cycle and then the final PASS handoff to the operator.
#[tokio::test]
async fn coding_team_review_loop_returns_changes_and_passes() {
    let _guard = FACTORY_LOCK.lock().await;
    install_mock_chain(vec![
        (
            "planner",
            vec![
                MockResponse::tool_use(
                    "p1",
                    "delegate_task",
                    r#"{"to": "reviewer", "task": "task-1: review the change", "timeout_secs": 60}"#,
                ),
                MockResponse::tool_use(
                    "p2",
                    "delegate_task",
                    r#"{"to": "coder", "task": "task-2: fix the review finding", "timeout_secs": 60}"#,
                ),
                MockResponse::tool_use(
                    "p3",
                    "delegate_task",
                    r#"{"to": "judge", "task": "review task-2 output", "timeout_secs": 60}"#,
                ),
            ],
        ),
        (
            "reviewer",
            vec![MockResponse::tool_use_end_turn(
                "r1",
                "send_message",
                r#"{"to": "judge", "body": "task-1 needs a naming fix"}"#,
            )],
        ),
        (
            "judge",
            vec![
                MockResponse::tool_use_end_turn(
                    "j1",
                    "send_message",
                    r##"{"to": "planner", "body": "NEEDS_CHANGES: fix the naming"}"##,
                ),
                MockResponse::tool_use_end_turn(
                    "j2",
                    "send_message",
                    r##"{"to": "operator", "body": "# Code Review 报告\n\n## 总体结论: PASS\n\n无 P0/P1 finding。"}"##,
                ),
            ],
        ),
        (
            "coder",
            vec![MockResponse::tool_use_end_turn(
                "c1",
                "send_message",
                r#"{"to": "planner", "task_id": "task-planner-1", "body": "task-2 fixed"}"#,
            )],
        ),
    ]);

    let repo = make_repo();
    let data_root = unique_dir("ct-e2e-review-loop");
    let spec = llm_harness_agent_team_studio::team_spec::TeamSpec {
        id: "review-loop".into(),
        template: None,
        template_baseline: None,
        name: "review-loop".into(),
        repo_path: Some(repo.to_string_lossy().to_string()),
        members: ["planner", "reviewer", "judge", "coder"]
            .iter()
            .map(|id| llm_harness_agent_team_studio::team_spec::MemberSpec {
                id: (*id).into(),
                persona: format!("{id} persona"),
                role_label: id.to_string(),
                model: llm_harness_agent_team_studio::team_spec::ModelRef::Explicit((*id).into()),
                toolkits: vec![],
                behaviors: vec![],
                provider_override: None,
                persona_file: None,
            })
            .collect(),
    };
    let mut pm = TeamManager::new(&data_root);
    pm.create(spec, Some(repo), &settings(), "").await.unwrap();

    pm.get("review-loop")
        .unwrap()
        .team
        .submit(
            "reviewer",
            "review task-1, apply changes if needed, then report",
        )
        .unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let operator_messages = {
            let ctx = pm
                .get("review-loop")
                .unwrap()
                .team
                .social_context()
                .unwrap()
                .clone();
            ctx.drain_inbox(&AgentId::new("operator"))
        };
        if operator_messages
            .iter()
            .any(|message| message.body.contains("总体结论: PASS"))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "review loop did not reach the operator within 10s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    pm.shutdown("review-loop").await.unwrap();
}

/// T1: chat → planner inbox receives the routed text (backend routing by
/// explicit target, the core of the operator entry point).
#[tokio::test]
async fn chat_routes_to_planner_inbox() {
    let _guard = FACTORY_LOCK.lock().await;
    install_mock(vec![]);
    let repo = make_repo();
    let data_root = unique_dir("ct-e2e");
    let mut pm = TeamManager::new(&data_root);
    pm.create(
        spec_with_scout("p", &repo, &data_root, 0),
        Some(repo),
        &settings(),
        "",
    )
    .await
    .unwrap();

    pm.get("p")
        .unwrap()
        .team
        .submit("planner", "修复 executor 的 IO 错误处理")
        .unwrap();

    let inbox = drain_planner(&RwLock::new(pm_rewrap(pm)), "p").await;
    assert!(
        inbox.iter().any(|b| b.contains("修复 executor")),
        "planner must receive the operator chat: {inbox:?}"
    );
}

// Helper: TeamManager moved into the async fn signature; RwLock wrapper
// mirrors the routes' SharedPm shape.
fn pm_rewrap(pm: TeamManager) -> TeamManager {
    pm
}

/// T2: confirmed issue hands the full body to the planner.
#[tokio::test]
async fn confirmed_issue_reaches_planner_with_body() {
    let _guard = FACTORY_LOCK.lock().await;
    install_mock(vec![]);
    let repo = make_repo();
    let data_root = unique_dir("ct-e2e");
    let mut pm = TeamManager::new(&data_root);
    pm.create(
        spec_with_scout("p", &repo, &data_root, 0),
        Some(repo),
        &settings(),
        "",
    )
    .await
    .unwrap();

    let (project_id, issues_root) = {
        let p = pm.get("p").unwrap();
        (p.id.clone(), p.issues.root().to_path_buf())
    };
    let store = llm_harness_agent_team_studio::issue::IssueStore::new(&issues_root);
    let issue = store
        .create(
            "scout",
            "executor 吞掉 IO 上下文",
            "现象：read 失败只剩 'io error'",
        )
        .unwrap();

    // Human confirms.
    store.set_status(&issue.id, IssueStatus::Confirmed).unwrap();

    // Handoff (what the route handler does on confirm).
    let confirmed = store.load(&issue.id).unwrap().0;
    let text = format!(
        "以下 issue 已由操作员确认，按其内容拆解任务：\n\nid: {}\ntitle: {}\n\n{}",
        confirmed.id, confirmed.title, confirmed.body
    );
    pm.get(&project_id)
        .unwrap()
        .team
        .submit("planner", &text)
        .unwrap();

    let inbox = drain_planner(&RwLock::new(pm), "p").await;
    assert!(
        inbox
            .iter()
            .any(|b| b.contains("操作员确认") && b.contains("read 失败只剩")),
        "planner must receive the full issue body: {inbox:?}"
    );
}

/// T3: scout arm → patrol task in the scout's inbox; scout mock writes an
/// issue file and schedules its own reminder (self-renewal contract).
#[tokio::test]
async fn scout_patrol_writes_issue_and_renews() {
    let _guard = FACTORY_LOCK.lock().await;
    // The issue dir must exist before the sequence is built (the mock
    // writes an absolute path into it, mirroring the real prompt contract).
    let data_root = unique_dir("ct-e2e");
    let issues_root = data_root.join("teams").join("p").join("issues");
    std::fs::create_dir_all(&issues_root).unwrap();

    let sequence = vec![
        // Scout writes the issue with an absolute path... Forward slashes
        // only: a Windows temp path contains backslashes, which would be
        // invalid JSON escapes inside the mock's raw args string (serde_json
        // then degrades the whole input to {"_parse_error", "raw"} and the
        // write never runs). Windows accepts forward slashes in paths.
        MockResponse::tool_use(
            "t1",
            "write",
            &format!(
                r#"{{"path": "{}/I-009-todo-debt.md", "content": "---\nid: I-009\nstatus: pending\nsource: scout\ncreated: 2026-08-30T00:00:00Z\ntitle: TODO debt in executor\n---\n五处 TODO 未跟踪"}}"#,
                issues_root.display().to_string().replace('\\', "/")
            ),
        ),
        // ...then schedules its own next patrol (self-renewal contract).
        MockResponse::tool_use_end_turn(
            "t2",
            "set_reminder",
            r#"{"reminder": "下次巡检", "delay_secs": 3600}"#,
        ),
    ];
    let repo = make_repo();
    install_mock(sequence);
    let mut pm = TeamManager::new(&data_root);
    // scout armed (interval > 0): the patrol task lands in its inbox.
    // Solo scout: no other member could consume the shared mock sequence.
    let mut solo = spec_with_scout("p", &repo, &data_root, 3600);
    solo.members.retain(|m| m.id == "scout");
    pm.create(solo, Some(repo), &settings(), "").await.unwrap();

    // Wait for the patrol run to produce the issue file.
    let store = llm_harness_agent_team_studio::issue::IssueStore::new(&issues_root);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if !store.list().unwrap().is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "scout never wrote the issue file"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let issues = store.list().unwrap();
    assert_eq!(issues[0].status, IssueStatus::Pending);
    assert_eq!(issues[0].source, "scout");
}

/// T4: isolation — two projects share nothing.
#[tokio::test]
async fn projects_are_fully_isolated() {
    let _guard = FACTORY_LOCK.lock().await;
    install_mock(vec![]);
    let repo_a = make_repo();
    let repo_b = make_repo();
    let data_root = unique_dir("ct-e2e");
    let mut pm = TeamManager::new(&data_root);
    pm.create(
        spec_with_scout("a", &repo_a, &data_root, 0),
        Some(repo_a),
        &settings(),
        "",
    )
    .await
    .unwrap();
    pm.create(
        spec_with_scout("b", &repo_b, &data_root, 0),
        Some(repo_b),
        &settings(),
        "",
    )
    .await
    .unwrap();

    pm.get("a")
        .unwrap()
        .issues
        .create("scout", "a-only", "x")
        .unwrap();

    // B's issue list is empty; A's has one.
    let (a_issues, b_issues) = {
        let pm_ref = &pm;
        (
            pm_ref.get("a").unwrap().issues.list().unwrap().len(),
            pm_ref.get("b").unwrap().issues.list().unwrap().len(),
        )
    };
    assert_eq!(a_issues, 1);
    assert_eq!(b_issues, 0);

    // Memory roots differ.
    assert_ne!(
        pm.get("a").unwrap().data_dir(),
        pm.get("b").unwrap().data_dir()
    );

    // A chat to A's planner never lands in B's planner inbox.
    pm.get("a")
        .unwrap()
        .team
        .submit("planner", "for a only")
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let b_inbox = drain_planner(&RwLock::new(pm), "b").await;
    assert!(b_inbox.is_empty(), "B planner must not see A's traffic");
}

/// T5: review chain — coder completion message flows planner-side; judge
/// mock renders a PASS verdict to the operator (message shape contract).
#[tokio::test]
async fn judge_pass_reaches_operator() {
    let _guard = FACTORY_LOCK.lock().await;
    // judge's scripted turn: read nothing, report PASS to the operator.
    let sequence = vec![MockResponse::tool_use_end_turn(
        "t1",
        "send_message",
        r##"{"to": "operator", "body": "# Code Review 报告\n\n## 总体结论: PASS\n\n无 P0/P1 finding。"}"##,
    )];
    let repo = make_repo();
    install_mock(sequence);
    let data_root = unique_dir("ct-e2e");
    let mut pm = TeamManager::new(&data_root);
    pm.create(
        spec_with_scout("p", &repo, &data_root, 0),
        Some(repo),
        &settings(),
        "",
    )
    .await
    .unwrap();

    // planner hands the review to the judge.
    pm.get("p")
        .unwrap()
        .team
        .submit("judge", "审查 coder 的任务 t-1 产出")
        .unwrap();

    // The judge harness runs its scripted turn; wait for the operator
    // message to arrive, then assert its shape.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let got = {
            let ctx = pm.get("p").unwrap().team.social_context().unwrap().clone();
            ctx.drain_inbox(&AgentId::new("operator"))
        };
        if got.iter().any(|m| m.body.contains("总体结论: PASS")) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "judge PASS never reached the operator"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

// ---------------------------------------------------------------------------
// Live LLM tests (require network access; run with `cargo test -- --ignored`)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires live LLM endpoint; run with --ignored"]
async fn live_llm_team_sends_and_receives() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .unwrap_or_else(|_| panic!("set ANTHROPIC_API_KEY to the local LLM token"));
    let base_url = "http://10.0.20.110:3001";
    let model = "moonshotai/kimi-k2.6";

    let repo = make_repo();
    let data_root = unique_dir("ct-live");
    let mut pm = TeamManager::new(&data_root);

    let settings = llm_harness_agent_team_studio::settings::Settings {
        models: ModelTier {
            strong: model.into(),
            main: model.into(),
            cheap: model.into(),
        },
        scout_interval_secs: 0,
        base_url: base_url.into(),
    };

    // Minimal single-member team with no tools — we just need prompt→response.
    let spec = llm_harness_agent_team_studio::team_spec::TeamSpec {
        id: "live".into(),
        template: None,
        template_baseline: None,
        name: "live".into(),
        repo_path: Some(repo.to_string_lossy().to_string()),
        members: vec![llm_harness_agent_team_studio::team_spec::MemberSpec {
            id: "planner".into(),
            persona: "你是 planner。收到任务后，用 send_message(\"operator\", ...) 回复一句确认。"
                .into(),
            role_label: "规划者".into(),
            model: llm_harness_agent_team_studio::team_spec::ModelRef::Explicit(model.into()),
            toolkits: vec![],
            behaviors: vec![],
            provider_override: None,
            persona_file: None,
        }],
    };
    pm.create(spec, Some(repo), &settings, &api_key)
        .await
        .unwrap();

    // Submit a task and wait for the operator inbox to receive a reply.
    pm.get("live")
        .unwrap()
        .team
        .submit("planner", "请确认收到，回复：confirmed")
        .unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    let ctx = pm
        .get("live")
        .unwrap()
        .team
        .social_context()
        .unwrap()
        .clone();
    loop {
        let got = ctx.drain_inbox(&AgentId::new("operator"));
        if !got.is_empty() {
            let body = &got[0].body;
            println!("operator received: {body}");
            assert!(!body.trim().is_empty(), "operator received empty message");
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "operator inbox empty after 120s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    pm.shutdown("live").await.unwrap();
}

#[tokio::test]
#[ignore = "requires live LLM endpoint; run with --ignored"]
async fn live_llm_review_loop_reaches_operator() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .unwrap_or_else(|_| panic!("set ANTHROPIC_API_KEY to the local LLM token"));
    let base_url = "http://10.0.20.110:3001";
    let model = "moonshotai/kimi-k2.6";

    let repo = make_repo();
    let data_root = unique_dir("ct-live-review-loop");
    let mut pm = TeamManager::new(&data_root);
    let settings = llm_harness_agent_team_studio::settings::Settings {
        models: ModelTier {
            strong: model.into(),
            main: model.into(),
            cheap: model.into(),
        },
        scout_interval_secs: 0,
        base_url: base_url.into(),
    };

    let persona =
        |text: &str| format!("你是 coding-team 成员。只执行当前消息，不要发明其他阶段。{text}");
    let spec = llm_harness_agent_team_studio::team_spec::TeamSpec {
        id: "live-review-loop".into(),
        template: None,
        template_baseline: None,
        name: "live-review-loop".into(),
        repo_path: Some(repo.to_string_lossy().to_string()),
        members: vec![
            llm_harness_agent_team_studio::team_spec::MemberSpec {
                id: "planner".into(),
                persona: persona(
                    "第一次收到用户请求时，立即用 delegate_task 派给 reviewer。收到 reviewer 或 coder 消息后，把该结果用 delegate_task 派给 reviewer；收到 judge 的 NEEDS_CHANGES 后，用 delegate_task 派给 coder。不要直接回复 operator。",
                ),
                role_label: "planner".into(),
                model: llm_harness_agent_team_studio::team_spec::ModelRef::Explicit(model.into()),
                toolkits: vec![],
                behaviors: vec![],
                provider_override: None,
                persona_file: None,
            },
            llm_harness_agent_team_studio::team_spec::MemberSpec {
                id: "reviewer".into(),
                persona: persona(
                    "立即用 send_message(\"judge\", ...) 汇报：NEEDS_CHANGES: simplify the response。",
                ),
                role_label: "reviewer".into(),
                model: llm_harness_agent_team_studio::team_spec::ModelRef::Explicit(model.into()),
                toolkits: vec![],
                behaviors: vec![],
                provider_override: None,
                persona_file: None,
            },
            llm_harness_agent_team_studio::team_spec::MemberSpec {
                id: "coder".into(),
                persona: persona(
                    "立即用 send_message(\"planner\", task_id, body) 汇报：task completed。",
                ),
                role_label: "coder".into(),
                model: llm_harness_agent_team_studio::team_spec::ModelRef::Explicit(model.into()),
                toolkits: vec![],
                behaviors: vec![],
                provider_override: None,
                persona_file: None,
            },
            llm_harness_agent_team_studio::team_spec::MemberSpec {
                id: "judge".into(),
                persona: persona(
                    "第一次收到消息时，立即用 send_message(\"planner\", ...) 发送：NEEDS_CHANGES: simplify the response。第二次收到消息时，立即用 send_message(\"operator\", ...) 发送正文必须包含：## 总体结论: PASS。",
                ),
                role_label: "judge".into(),
                model: llm_harness_agent_team_studio::team_spec::ModelRef::Explicit(model.into()),
                toolkits: vec![],
                behaviors: vec![],
                provider_override: None,
                persona_file: None,
            },
        ],
    };
    pm.create(spec, Some(repo), &settings, &api_key)
        .await
        .unwrap();
    pm.get("live-review-loop")
        .unwrap()
        .team
        .submit("planner", "run the review loop for task-1")
        .unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(240);
    let ctx = pm
        .get("live-review-loop")
        .unwrap()
        .team
        .social_context()
        .unwrap()
        .clone();
    loop {
        let got = ctx.drain_inbox(&AgentId::new("operator"));
        if let Some(message) = got
            .iter()
            .find(|message| message.body.contains("总体结论: PASS"))
        {
            println!("operator received: {}", message.body);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "review loop did not reach the operator within 240s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    pm.shutdown("live-review-loop").await.unwrap();
}

#[tokio::test]
#[ignore = "requires live LLM endpoint and local git; run with --ignored"]
async fn live_llm_full_coding_team_template_completes_task() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .unwrap_or_else(|_| panic!("set ANTHROPIC_API_KEY to the local LLM token"));
    let base_url = "http://10.0.20.110:3001";
    let model = "moonshotai/kimi-k2.6";

    let repo = unique_dir("ct-live-full-repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init"]);
    git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(&["config", "user.name", "Agent Team E2E"]);
    git(&["config", "user.email", "agent-team-e2e@example.invalid"]);
    std::fs::write(repo.join("README.md"), "# Agent Team E2E\n").unwrap();
    git(&["add", "README.md"]);
    git(&["commit", "-m", "initial"]);

    let mut spec = llm_harness_agent_team_studio::template_store::TemplateStore::new(None)
        .load(llm_harness_agent_team_studio::template_store::BUILTIN_CODING_TEAM_ID)
        .unwrap()
        .spec;
    for member in &mut spec.members {
        member.model = llm_harness_agent_team_studio::team_spec::ModelRef::Explicit(model.into());
    }

    let data_root = unique_dir("ct-live-full");
    println!("data_root: {}", data_root.display());
    println!("repo: {}", repo.display());
    let mut pm = TeamManager::new(&data_root);
    let settings = llm_harness_agent_team_studio::settings::Settings {
        models: ModelTier {
            strong: model.into(),
            main: model.into(),
            cheap: model.into(),
        },
        scout_interval_secs: 0,
        base_url: base_url.into(),
    };
    assert_eq!(spec.members.len(), 11);
    pm.create(spec, Some(repo.clone()), &settings, &api_key)
        .await
        .unwrap();
    pm.get("coding-team")
        .unwrap()
        .team
        .submit(
            "planner",
            "bounded task: create answer.txt containing exactly 42 and verify it with `test \"$(cat answer.txt)\" = 42`. Record the plan in docs/plans/e2e.md, use team/e2e, commit the result, then run the standard review loop and report PASS or NEEDS_CHANGES to operator.",
        )
        .unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(900);
    let ctx = pm
        .get("coding-team")
        .unwrap()
        .team
        .social_context()
        .unwrap()
        .clone();
    loop {
        let messages = ctx.drain_inbox(&AgentId::new("operator"));
        let is_final_pass = |message: &llm_harness_agent_team::InboxMessage| {
            message.from.as_str() == "judge"
                && message.body.contains("PASS")
                && message.body.contains("裁决明细")
                && !message.body.contains("状态更新")
                && !message.body.contains("NEEDS_CHANGES")
        };
        if let Some(message) = messages.iter().find(|message| is_final_pass(message)) {
            println!("operator received: {}", message.body);
            break;
        }
        for message in &messages {
            println!(
                "operator progress from {}: {}",
                message.from.as_str(),
                message.body
            );
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "full coding-team task did not reach PASS within 900s"
        );
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
    let artifact = std::fs::read_to_string(repo.join("answer.txt")).unwrap();
    assert_eq!(artifact.trim(), "42");
    let branch = String::from_utf8(
        std::process::Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(&repo)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(branch.trim(), "team/e2e");
    pm.shutdown("coding-team").await.unwrap();
}
#[cfg(unix)]
mod binary_lifecycle_tests {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    #[test]
    fn agent_studio_reports_private_descriptor_and_cleanly_stops() {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let data_root = std::env::temp_dir().join(format!(
            "agent-studio-binary-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&data_root);
        std::fs::create_dir_all(&data_root).unwrap();

        let mut child = Command::new(env!("CARGO_BIN_EXE_agent-studio"))
            .env("STUDIO_DATA_ROOT", &data_root)
            .env("STUDIO_PORT", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let stdout = child.stdout.take().unwrap();
        let mut stdout_reader = BufReader::new(stdout);
        let mut descriptor_line = String::new();
        let started = Instant::now();
        loop {
            descriptor_line.clear();
            match stdout_reader.read_line(&mut descriptor_line) {
                Ok(0) => panic!("agent-studio exited before reporting its panel descriptor"),
                Ok(_) => {}
                Err(error) => panic!("failed to read agent-studio stdout: {error}"),
            }
            if descriptor_line.starts_with("STUDIO_PANEL_DESCRIPTOR=") {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(30));
        }
        let descriptor_path: std::path::PathBuf = descriptor_line
            .strip_prefix("STUDIO_PANEL_DESCRIPTOR=")
            .unwrap()
            .trim_end_matches(['\n', '\r'])
            .into();
        assert_eq!(descriptor_path, data_root.join("panel.json"));

        let descriptor: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&descriptor_path).unwrap()).unwrap();
        assert_eq!(
            descriptor["schema"],
            "llm-harness.studio.panel-descriptor.v1"
        );
        let panel_url = descriptor["url"].as_str().unwrap();
        assert!(panel_url.starts_with("http://127.0.0.1:"));
        assert!(panel_url.contains("/app/team.html#token="));
        let token = panel_url.rsplit("#token=").next().unwrap();
        assert!(!token.is_empty());
        assert!(!descriptor_line.contains("#token="));
        assert!(!data_root.join("panel.json.tmp").exists());

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&descriptor_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        let pid = child.id();
        let sent = unsafe { libc::kill(pid.try_into().unwrap(), libc::SIGTERM) };
        assert_eq!(sent, 0, "failed to send SIGTERM to agent-studio");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "agent-studio exited with {status}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "agent-studio did not stop within 10s"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(!descriptor_path.exists());
        assert!(!data_root.join("panel.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&data_root);
    }
}
