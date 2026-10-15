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
        sandbox_mode: None,
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
        sandbox_mode: None,
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
        sandbox_mode: None,
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
// V0 team-composition contract — the coding-team template must instantiate
// exactly the intended V0 member set, and the runtime member list (what the
// Studio panel and dispatch whitelists see) must equal the spec.
// ---------------------------------------------------------------------------

/// The intended V0 coding-team member set: exactly the agents the
/// `pipeline.rs` execution graph requires — implementer and one independent
/// reviewer (`is_reviewer_id` prefix match). Planner, judge, and the extra
/// `reviewer-*` variants were removed; brainstormer/debugger/scout are NOT
/// dispatched, awaited, or selected by the V0 pipeline and are not
/// instantiated by the template (their prompt files remain in the
/// template store for user-authored specs).
const V0_TEMPLATE_MEMBERS: [&str; 3] = ["orchestrator", "coder", "reviewer"];

#[tokio::test]
async fn v0_coding_team_template_composes_exactly_the_intended_members() {
    let _guard = FACTORY_LOCK.lock().await;
    // No LLM calls needed: a fresh team's members stay Idle. The mock
    // factory is still required by team build; an empty sequence is fine.
    install_mock(vec![]);

    let repo = make_repo();
    let data_root = unique_dir("ct-v0-composition");

    // Template source of truth: the builtin coding-team template expands to
    // exactly the V0 member set, no planner, no duplicates.
    let store = llm_harness_agent_team_studio::template_store::TemplateStore::new(Some(
        data_root.join("templates"),
    ));
    let template = store
        .load("coding-team")
        .expect("builtin coding-team template must load");
    let template_ids: Vec<&str> = template
        .spec
        .members
        .iter()
        .map(|m| m.id.as_str())
        .collect();
    assert_eq!(template_ids, V0_TEMPLATE_MEMBERS);
    assert!(
        !template_ids.contains(&"planner"),
        "V0 composition: planner must not be part of the coding-team template"
    );

    // Creation path: register the team from the template exactly as the
    // Studio route does, then verify the RUNTIME member set (harnesses +
    // social registry) equals the spec — no stale/legacy extras.
    let mut spec = template.spec.clone();
    spec.id = "v0-comp".into();
    spec.repo_path = Some(repo.to_string_lossy().to_string());
    spec.template = Some(llm_harness_agent_team_studio::team_spec::TemplateRef {
        template_id: "coding-team".into(),
        version: template.version.clone(),
    });
    spec.template_baseline = Some(llm_harness_agent_team_studio::team_spec::TemplateBaseline {
        version: template.version.clone(),
        members: spec.members.clone(),
    });
    let mut pm = TeamManager::new(&data_root);
    pm.create(spec, Some(repo), &settings(), "").await.unwrap();

    let entry = pm.get("v0-comp").unwrap();
    let ctx = entry.team.social_context().unwrap().clone();
    let expected: std::collections::HashSet<&str> = V0_TEMPLATE_MEMBERS.into();

    // Every spec member has a built harness (the runner's execution unit).
    for id in V0_TEMPLATE_MEMBERS {
        assert!(
            entry.team.harness(&AgentId::new(id)).is_some(),
            "member {id} from the template must have a runtime harness"
        );
        assert!(
            ctx.get_agent_info(&AgentId::new(id)).is_some(),
            "member {id} must be registered in the shared social context"
        );
    }
    // No extra runtime members beyond the template set.
    let runtime_ids: Vec<String> = entry.spec.members.iter().map(|m| m.id.clone()).collect();
    let runtime_set: std::collections::HashSet<&str> =
        runtime_ids.iter().map(String::as_str).collect();
    assert_eq!(
        runtime_set, expected,
        "runtime member set must equal the intended V0 set"
    );
    // Spot-check the removed role: planner is neither registered nor
    // addressable through the team submit whitelist.
    assert!(ctx.get_agent_info(&AgentId::new("planner")).is_none());
    assert!(
        pm.get("v0-comp")
            .unwrap()
            .team
            .submit("planner", "x")
            .is_err()
    );

    // Persistence round-trip: restart rebuilds the same member set, so no
    // stale members can accumulate across restarts.
    let persisted_ids: Vec<String> =
        llm_harness_agent_team_studio::team_manager::list_persisted(&data_root)
            .into_iter()
            .find(|s| s.id == "v0-comp")
            .map(|s| s.members.iter().map(|m| m.id.clone()).collect())
            .unwrap_or_default();
    let persisted_set: std::collections::HashSet<&str> =
        persisted_ids.iter().map(String::as_str).collect();
    assert_eq!(
        persisted_set, expected,
        "persisted spec must keep the V0 set"
    );
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
        sandbox_mode: None,
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
        sandbox_mode: None,
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
        assert!(panel_url.contains("/#token="));
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

#[cfg(unix)]
mod desktop_host_tests {
    use llm_harness_agent_team_studio::host::{StudioBackendHost, StudioHostConfig};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temp_root(test: &str) -> PathBuf {
        let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "agent-studio-desktop-host-{test}-{}-{sequence}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn host_supervises_ready_backend_and_clean_shutdown() {
        let root = temp_root("ready");
        std::fs::create_dir_all(&root).unwrap();
        let host = StudioBackendHost::spawn(StudioHostConfig::new(
            env!("CARGO_BIN_EXE_agent-studio"),
            &root,
        ))
        .unwrap();
        let backend = host.wait_ready().await.unwrap();
        assert_eq!(backend.descriptor_path(), root.join("panel.json"));
        assert!(backend.backend_pid().is_some());

        let shutdown = backend.shutdown().await.unwrap();
        assert!(shutdown.status.success());
        assert!(!shutdown.forced);
        assert!(!root.join("panel.json").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn host_surfaces_duplicate_data_root_instance() {
        let root = temp_root("duplicate");
        std::fs::create_dir_all(&root).unwrap();
        let first = StudioBackendHost::spawn(StudioHostConfig::new(
            env!("CARGO_BIN_EXE_agent-studio"),
            &root,
        ))
        .unwrap()
        .wait_ready()
        .await
        .unwrap();

        let duplicate = StudioBackendHost::spawn(StudioHostConfig::new(
            env!("CARGO_BIN_EXE_agent-studio"),
            &root,
        ))
        .unwrap()
        .wait_ready()
        .await;
        assert!(matches!(
            duplicate,
            Err(llm_harness_agent_team_studio::host::HostError::BackendExit(
                status,
            )) if !status.success()
        ));

        first.shutdown().await.unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn host_surfaces_backend_exit_before_readiness() {
        let root = temp_root("unexpected-exit");
        std::fs::create_dir_all(&root).unwrap();
        let result = StudioBackendHost::spawn(StudioHostConfig::new("/bin/false", &root))
            .unwrap()
            .wait_ready()
            .await;
        assert!(matches!(
            result,
            Err(llm_harness_agent_team_studio::host::HostError::BackendExit(
                status,
            )) if !status.success()
        ));
        std::fs::remove_dir_all(&root).unwrap();
    }
}

// ---------------------------------------------------------------------------
// Orchestrator decision pipeline — every semantic advance is an ACTION
// ---------------------------------------------------------------------------

use parking_lot::Mutex;

use std::collections::VecDeque;

use llm_harness_agent_team_studio::pipeline::{PipelineController, PipelinePhase};
use llm_harness_agent_team_studio::team_spec::{
    MemberSpec, ModelRef, TeamSpec, TemplateBaseline, TemplateRef,
};

/// Ordered ledger of pipeline lifecycle events observed through the
/// controller's observability sink. Included verbatim in failure output.
#[derive(Default)]
struct StageLedger {
    events: parking_lot::Mutex<Vec<(String, serde_json::Value)>>,
}

impl StageLedger {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn record(&self, event: &str, payload: &serde_json::Value) {
        self.events
            .lock()
            .push((event.to_string(), payload.clone()));
    }
}

/// Hard test-side bound for a whole workflow run. Mock-driven agent turns
/// finish in well under a second, so a broken round fails here in seconds
/// with stage evidence instead of hanging for the production 600 s wait.
const STAGE_BOUND: std::time::Duration = std::time::Duration::from_secs(8);

/// Synthetic controller identity; mirrors the private `pipeline::CONTROLLER`.
const CONTROLLER_ID: &str = "pipeline-controller";

/// Deterministic echo mock: replies to each dispatch through the REAL
/// send_message path, echoing back the runtime-generated task id the
/// dispatch body demanded (`task_id="<id>"`). Scripted bodies are consumed
/// in order, one per agent run. The substrate loop contract requires a
/// trailing TEXT turn after each EndTurn tool call, so responses alternate
/// tool-reply → text.
struct EchoClient {
    bodies: Mutex<VecDeque<String>>,
    task_kinds: Mutex<VecDeque<String>>,
    /// Agent runs that produced a correlated send_message reply.
    replies: std::sync::atomic::AtomicUsize,
    call_count: std::sync::atomic::AtomicUsize,
    captured_requests: Mutex<Vec<llm_harness_loop::ChatRequest>>,
}

#[derive(Clone, Copy)]
enum WorkerLifecycleScript {
    Implement,
    Review,
}

/// Worker mock that performs a real non-message `read` tool call before its
/// terminal report. The implementer also sends correlated progress and pauses
/// after delivery so the test can prove the controller has not advanced.
struct WorkThenReportClient {
    script: WorkerLifecycleScript,
    calls: std::sync::atomic::AtomicUsize,
    choices: Mutex<Vec<String>>,
    resume_after_progress: tokio::sync::Notify,
}

impl WorkThenReportClient {
    fn new(script: WorkerLifecycleScript) -> Arc<Self> {
        Arc::new(Self {
            script,
            calls: Default::default(),
            choices: Mutex::new(vec![]),
            resume_after_progress: tokio::sync::Notify::new(),
        })
    }
}

impl llm_harness_loop::LlmClient for WorkThenReportClient {
    fn capabilities(&self) -> llm_harness_loop::ProviderCapabilities {
        llm_harness_loop::ProviderCapabilities::new(false, false, false)
    }

    fn chat<'a, 'b, 'f>(
        &'a self,
        req: &'b llm_harness_loop::ChatRequest,
    ) -> futures::future::BoxFuture<
        'f,
        Result<llm_harness_loop::ChatResponse, llm_harness_loop::LlmError>,
    >
    where
        'a: 'f,
        'b: 'f,
        Self: 'f,
    {
        Box::pin(async move { self.chat_stream(req).await?.collect().await })
    }

    fn chat_stream<'a, 'b, 'f>(
        &'a self,
        req: &'b llm_harness_loop::ChatRequest,
    ) -> futures::future::BoxFuture<
        'f,
        Result<llm_harness_loop::StreamHandle, llm_harness_loop::LlmError>,
    >
    where
        'a: 'f,
        'b: 'f,
        Self: 'f,
    {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.choices.lock().push(format!("{:?}", req.tool_choice()));
            let task = flatten_request(req)
                .rsplit_once("task_id=\"")
                .expect("controlled dispatch carries task id")
                .1
                .split('"')
                .next()
                .unwrap()
                .to_string();
            let response = match (self.script, call) {
                (_, 0) => MockResponse::tool_use("inspect", "read", r#"{"path":"."}"#),
                (WorkerLifecycleScript::Implement, 1) => MockResponse::tool_use(
                    "progress",
                    "send_message",
                    &serde_json::json!({
                        "to": CONTROLLER_ID,
                        "task_id": task,
                        "task_kind": "progress",
                        "body": "Acknowledged progress: repository inspected; tests next.",
                    })
                    .to_string(),
                ),
                (WorkerLifecycleScript::Implement, 2) => {
                    self.resume_after_progress.notified().await;
                    MockResponse::tool_use("verify", "read", r#"{"path":"."}"#)
                }
                (WorkerLifecycleScript::Implement, 3) => MockResponse::tool_use(
                    "complete",
                    "send_message",
                    &serde_json::json!({
                        "to": CONTROLLER_ID,
                        "task_id": task,
                        "task_kind": "result",
                        "body": "implementation evidence and tests passed",
                    })
                    .to_string(),
                ),
                (WorkerLifecycleScript::Implement, 4) | (WorkerLifecycleScript::Review, 3) => {
                    MockResponse::text("report delivered")
                }
                (WorkerLifecycleScript::Review, 1) => MockResponse::tool_use(
                    "review-complete",
                    "send_message",
                    &serde_json::json!({
                        "to": CONTROLLER_ID,
                        "task_id": task,
                        "task_kind": "result",
                        "body": "no findings; inspected repository evidence",
                    })
                    .to_string(),
                ),
                (WorkerLifecycleScript::Review, 2) => MockResponse::text("report delivered"),
                _ => panic!("unexpected controlled worker provider call {call}"),
            };
            MockLlmClient::with_single(response).chat_stream(req).await
        })
    }
}

impl EchoClient {
    fn new(bodies: &[&str]) -> Arc<Self> {
        Self::new_with_kinds(bodies, &vec!["result"; bodies.len()])
    }

    fn new_with_kinds(bodies: &[&str], task_kinds: &[&str]) -> Arc<Self> {
        assert_eq!(bodies.len(), task_kinds.len());
        Arc::new(Self {
            bodies: Mutex::new(bodies.iter().map(|s| s.to_string()).collect()),
            task_kinds: Mutex::new(task_kinds.iter().map(|s| s.to_string()).collect()),
            replies: Default::default(),
            call_count: Default::default(),
            captured_requests: Mutex::new(vec![]),
        })
    }
}

impl llm_harness_loop::LlmClient for EchoClient {
    fn capabilities(&self) -> llm_harness_loop::ProviderCapabilities {
        llm_harness_loop::ProviderCapabilities::new(false, false, false)
    }

    fn chat<'a, 'b, 'f>(
        &'a self,
        req: &'b llm_harness_loop::ChatRequest,
    ) -> futures::future::BoxFuture<
        'f,
        Result<llm_harness_loop::ChatResponse, llm_harness_loop::LlmError>,
    >
    where
        'a: 'f,
        'b: 'f,
        Self: 'f,
    {
        Box::pin(async move { self.chat_stream(req).await?.collect().await })
    }

    fn chat_stream<'a, 'b, 'f>(
        &'a self,
        req: &'b llm_harness_loop::ChatRequest,
    ) -> futures::future::BoxFuture<
        'f,
        Result<llm_harness_loop::StreamHandle, llm_harness_loop::LlmError>,
    >
    where
        'a: 'f,
        'b: 'f,
        Self: 'f,
    {
        Box::pin(async move {
            use llm_harness_loop::test_utils::MockResponse;
            self.call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.captured_requests.lock().push(req.clone());
            // Decision sealing makes the correlated send_message the run's
            // final tool action: every scripted call pops the next body and
            // replies through the correlated tool, and the run ends there.
            let body = self
                .bodies
                .lock()
                .pop_front()
                .expect("scripted echo bodies exhausted");
            let task_kind = self
                .task_kinds
                .lock()
                .pop_front()
                .expect("scripted echo task kinds exhausted");
            self.replies
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let text = flatten_request(req);
            let task = text
                .rsplit_once("task_id=\"")
                .expect("dispatch must carry task_id=\"...\"")
                .1
                .split('"')
                .next()
                .unwrap()
                .to_string();
            let response = MockResponse::tool_use_end_turn(
                "reply",
                "send_message",
                &serde_json::json!({
                    "to": CONTROLLER_ID, "task_id": task,
                    "task_kind": task_kind, "body": body,
                })
                .to_string(),
            );
            llm_harness_loop::test_utils::MockLlmClient::with_single(response)
                .chat_stream(req)
                .await
        })
    }
}

fn flatten_request(req: &llm_harness_loop::ChatRequest) -> String {
    req.messages()
        .iter()
        .filter_map(|m| match m {
            llm_harness_loop::LlmMessage::User(c) => Some(
                c.iter()
                    .filter_map(|p| match p {
                        llm_harness_loop::RequestContent::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn install_providers(providers: Vec<(&str, Arc<dyn llm_harness_loop::LlmClient>)>) {
    let providers: Vec<(String, Arc<dyn llm_harness_loop::LlmClient>)> = providers
        .into_iter()
        .map(|(id, client)| (id.to_owned(), client))
        .collect();
    llm_harness_agent_team_studio::env_cfg::install_provider_factory(Arc::new(move || {
        providers.clone()
    }));
}

/// Four-member decision team: the orchestrator plus three workers. The
/// controller (not any member) owns the workflow; the orchestrator is the
/// only member that emits ACTIONs.
fn decision_spec(id: &str, repo: &std::path::Path) -> TeamSpec {
    TeamSpec {
        id: id.into(),
        template: None,
        template_baseline: None,
        name: id.into(),
        repo_path: Some(repo.to_string_lossy().to_string()),
        sandbox_mode: None,
        members: ["orchestrator", "coder", "reviewer-robustness", "judge"]
            .iter()
            .map(|m| MemberSpec {
                id: (*m).into(),
                persona: format!("{m} persona"),
                role_label: m.to_string(),
                model: ModelRef::Explicit((*m).into()),
                toolkits: vec![],
                behaviors: vec![],
                provider_override: None,
                persona_file: None,
            })
            .collect(),
    }
}

fn event_sequence(ledger: &StageLedger) -> Vec<String> {
    ledger
        .events
        .lock()
        .iter()
        .map(|(e, p)| match e.as_str() {
            "task_delegated" => format!("dispatch:{}", p["to"].as_str().unwrap_or("?")),
            "worker_started" => format!("started:{}", p["worker"].as_str().unwrap_or("?")),
            "revision_requested" => format!("revise:{}", p["to"].as_str().unwrap_or("?")),
            "worker_completed" => format!("done:{}", p["worker"].as_str().unwrap_or("?")),
            other => other.to_string(),
        })
        .collect()
}

fn ledger_dump(ledger: &StageLedger) -> String {
    ledger
        .events
        .lock()
        .iter()
        .map(|(e, p)| format!("\n  {e}: {p}"))
        .collect()
}

/// A successful coder result has one mandatory controller-owned transition:
///
///   decision-1: ACTION: DISPATCH coder (round-1 assignment)
///   → coder runs, returns correlated evidence
///   → snapshot → reviewer → review evidence
///   decision-2: ACTION: DISPATCH coder (revision = re-dispatch + new assignment)
///   → snapshot → reviewer → review evidence
///   decision-3: ACTION: FINISH
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn v0_coder_result_requires_review_before_next_decision() {
    let _guard = FACTORY_LOCK.lock().await;
    let repo = make_repo();
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(repo.join(".gitignore"), ".env\n").unwrap();
    std::fs::write(repo.join(".env"), "SECRET=local\n").unwrap();
    std::fs::write(repo.join("source.py"), "captured source\n").unwrap();
    std::fs::write(repo.join("test_source.py"), "captured test\n").unwrap();

    let orchestrator_client = EchoClient::new(&[
        "ACTION: DISPATCH coder\nRound-1 assignment: implement the feature and report evidence.",
        "ACTION: DISPATCH coder\nRevision assignment: address the gap in the round-1 evidence and report again.",
        "ACTION: FINISH\nFinal response.",
    ]);
    let coder_client = EchoClient::new(&[
        "round-1 evidence: feature implemented per assignment",
        "round-2 evidence: revision applied per assignment",
    ]);
    let reviewer_client = EchoClient::new(&[
        "round-1 review: one P1 finding",
        "round-2 review: no findings",
    ]);
    let judge_client = EchoClient::new(&[]);
    install_providers(vec![
        ("orchestrator", orchestrator_client.clone()),
        ("coder", coder_client.clone()),
        ("reviewer-robustness", reviewer_client.clone()),
        ("judge", judge_client.clone()),
    ]);

    let data_root = unique_dir("ct-v0-decisions");
    let mut pm = TeamManager::new(&data_root);
    pm.create(
        decision_spec("v0", &repo),
        Some(repo.clone()),
        &settings(),
        "",
    )
    .await
    .unwrap();
    let (ctx, members, issues_root) = {
        let entry = pm.get("v0").unwrap();
        let members: Vec<String> = entry.spec.members.iter().map(|m| m.id.clone()).collect();
        (
            entry.team.social_context().unwrap().clone(),
            members,
            entry.issues.root().to_path_buf(),
        )
    };
    let ledger = StageLedger::new();
    let sink_ledger = Arc::clone(&ledger);
    let controller = Arc::new(
        PipelineController::new("v0", ctx.clone(), repo.clone(), &members, 3).with_sink(Arc::new(
            move |event: &str, payload: &serde_json::Value| {
                sink_ledger.record(event, payload);
            },
        )),
    );

    let store = llm_harness_agent_team_studio::issue::IssueStore::new(&issues_root);
    let issue = store
        .create(
            "scout",
            "implement feature X",
            "add a feature per the objective",
        )
        .unwrap();
    store.set_status(&issue.id, IssueStatus::Confirmed).unwrap();
    let confirmed = store.load(&issue.id).unwrap().0;

    let run_handle = tokio::spawn({
        let controller = Arc::clone(&controller);
        let issue = confirmed.clone();
        async move { (*controller).run(&issue).await }
    });
    let run_outcome = match tokio::time::timeout(STAGE_BOUND, run_handle).await {
        Ok(joined) => joined,
        Err(_) => panic!(
            "workflow did not reach a terminal state within {STAGE_BOUND:?}\n{}",
            ledger_dump(&ledger)
        ),
    };
    run_outcome
        .unwrap_or_else(|e| panic!("workflow task panicked: {e}\n{}", ledger_dump(&ledger)))
        .unwrap_or_else(|e| panic!("workflow run failed: {e}\n{}", ledger_dump(&ledger)));

    // Terminal state: the orchestrator's FINISH ended the workflow.
    let state = controller.state(&confirmed.id).unwrap();
    assert_eq!(state.phase, PipelinePhase::Done.as_str(), "{state:?}");
    assert_eq!(
        state.terminal_reason.as_deref(),
        Some("orchestrator finished")
    );
    assert_eq!(state.review_round, 2);
    assert!(state.snapshot_id.is_some(), "review requires a snapshot");
    assert_eq!(state.reviewers_dispatched, ["reviewer-robustness"]);
    assert_eq!(
        state.reviewer_findings,
        [(
            "reviewer-robustness".into(),
            "round-2 review: no findings".into()
        )]
    );
    let package = &state.terminal_user_outcome.as_ref().unwrap().body;
    let snapshots = ledger
        .events
        .lock()
        .iter()
        .filter(|(event, _)| event == "snapshot_collected")
        .map(|(_, payload)| payload["snapshot_id"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(snapshots.len(), 2);
    let run_root = repo.join(".pipeline/snapshots");
    let run_dirs: Vec<_> = std::fs::read_dir(&run_root).unwrap().map(|entry| entry.unwrap().path()).collect();
    assert_eq!(run_dirs.len(), 1, "one task-start baseline per workflow");
    let baseline_path = run_dirs[0].join("r0/snapshot.json");
    let baseline_before = std::fs::read(&baseline_path).unwrap();
    for round in [1, 2] {
        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(run_dirs[0].join(format!("r{round}/snapshot.json"))).unwrap()
        ).unwrap();
        assert_eq!(manifest["task_start_baseline"], baseline_path.to_string_lossy().as_ref());
        assert_eq!(manifest["task_relative_changed_paths"], serde_json::json!([]));
    }
    assert_eq!(std::fs::read(&baseline_path).unwrap(), baseline_before);
    assert!(package.contains(&format!("r1 / {} · coder report", snapshots[0])));
    assert!(package.contains(&format!("r2 / {} · coder report", snapshots[1])));
    assert!(package.contains("round-1 review: one P1 finding"));
    assert!(package.contains("resolution of these findings is unverified"));
    assert!(package.contains("round-2 review: no findings"));
    assert!(package.contains("Coder reports precede Snapshot capture"));

    // Exact causal chain: each successful coder result is followed by a
    // snapshot and reviewer before another orchestrator decision can run.
    assert_eq!(
        event_sequence(&ledger),
        vec![
            "pipeline_started".to_string(),
            "dispatch:coder".to_string(),
            "started:coder".to_string(),
            "done:coder".to_string(),
            "snapshot_collected".to_string(),
            "dispatch:reviewer-robustness".to_string(),
            "started:reviewer-robustness".to_string(),
            "done:reviewer-robustness".to_string(),
            "revise:coder".to_string(),
            "dispatch:coder".to_string(),
            "started:coder".to_string(),
            "done:coder".to_string(),
            "snapshot_collected".to_string(),
            "dispatch:reviewer-robustness".to_string(),
            "started:reviewer-robustness".to_string(),
            "done:reviewer-robustness".to_string(),
            "pipeline_completed".to_string(),
        ],
        "every advance must come from an orchestrator action: {}",
        ledger_dump(&ledger),
    );

    // Round/artifact association: every event carries the issue id, and the
    // round-scoped events bind to the exact snapshot of their review round.
    {
        let events = ledger.events.lock();
        assert!(events.iter().all(|(_, p)| p["issue_id"].is_string()));
        let reviews: Vec<serde_json::Value> = events
            .iter()
            .filter(|(e, _)| e == "task_delegated")
            .filter(|(_, p)| p["snapshot_id"].is_string())
            .map(|(_, p)| p.clone())
            .collect();
        assert_eq!(reviews.len(), 2);
        assert_eq!(reviews[0]["review_round"], 1);
        assert_eq!(reviews[1]["review_round"], 2);
        let done: Vec<serde_json::Value> = events
            .iter()
            .filter(|(e, p)| e == "worker_completed" && p["snapshot_id"].is_string())
            .map(|(_, p)| p.clone())
            .collect();
        assert_eq!(done.len(), 2);
        assert_eq!(done[0]["snapshot_id"], reviews[0]["snapshot_id"]);
        assert_eq!(done[1]["snapshot_id"], reviews[1]["snapshot_id"]);
        let revision = events
            .iter()
            .find(|(e, _)| e == "revision_requested")
            .map(|(_, p)| p.clone())
            .expect("revision round must emit revision_requested");
        assert_eq!(revision["review_round"], 1);
        assert_eq!(revision["to"], "coder");
    }

    let seq = std::sync::atomic::Ordering::SeqCst;
    assert_eq!(orchestrator_client.replies.load(seq), 3);
    assert_eq!(coder_client.replies.load(seq), 2);
    assert_eq!(
        reviewer_client.replies.load(seq),
        2,
        "every successful coder result must be independently reviewed"
    );
    assert_eq!(
        judge_client.replies.load(seq),
        0,
        "judge must never auto-run on worker completion"
    );

    // Evidence flowed back: the orchestrator's later decisions carry the
    // worker's evidence, not a fixed chain's artifacts.
    {
        let reqs = orchestrator_client.captured_requests.lock();
        let all = reqs
            .iter()
            .map(|r| flatten_request(r))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all.contains("Previous result/evidence"),
            "orchestrator decisions receive worker evidence"
        );
        assert!(
            all.contains("round-1 evidence"),
            "orchestrator sees the worker's evidence before the next decision: {all}"
        );
        assert!(
            all.contains("round-1 review: one P1 finding"),
            "the next decision must include reviewer evidence: {all}"
        );
    }
    // Revision is expressed as a re-DISPATCH with a new bounded assignment.
    {
        let reqs = coder_client.captured_requests.lock();
        let all = reqs
            .iter()
            .map(|r| flatten_request(r))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all.contains("Round-1 assignment"),
            "coder receives the orchestrator's bounded assignment"
        );
        assert!(
            all.contains("Revision assignment"),
            "revision is a re-dispatch with a new assignment"
        );
    }
    {
        let reqs = reviewer_client.captured_requests.lock();
        assert_eq!(
            reqs.len(),
            2,
            "each reviewer terminal ends its controlled worker run"
        );
        let first = flatten_request(&reqs[0]);
        assert!(
            first.contains("Snapshot ID:"),
            "review gets snapshot identity"
        );
        assert!(
            first.contains("Snapshot evidence directory:"),
            "review gets persisted snapshot evidence"
        );
        for (round, request) in reqs.iter().enumerate() {
            let assignment = flatten_request(request);
            assert!(
                assignment.contains(&format!("Snapshot ID: {}", snapshots[round])),
                "reviewer must receive the current round's Snapshot identity"
            );
            let captured_dir = assignment
                .rsplit_once("Captured Snapshot files directory: ")
                .and_then(|(_, rest)| rest.lines().next())
                .expect("reviewer assignment names captured files directory");
            assert!(captured_dir.ends_with(&format!("/r{}/files", round + 1)));
            assert!(
                assignment
                    .contains("Read source and tests from the captured Snapshot files directory")
            );
            assert!(assignment.contains("Capture policy: git_ignore_aware"));
            assert!(assignment.contains("Excluded ignored paths: 1 total"));
            assert!(assignment.contains(".env"));
            assert!(!std::path::Path::new(captured_dir).join(".env").exists());
            assert_eq!(
                std::fs::read(std::path::Path::new(captured_dir).join("source.py")).unwrap(),
                b"captured source\n"
            );
        }
        assert!(
            first.contains("round-1 evidence"),
            "review gets coder evidence and the controlled task"
        );
    }

    // Planner remains absent.
    assert!(
        ctx.get_agent_info(&AgentId::new("planner")).is_none(),
        "planner must not be a registered team member"
    );
}

/// V0.6: the production ingress (`TeamEntry::start_workflow`) must attach the
/// controller's lifecycle sink to the project's EventBus, so the panel WS
/// stream (plus its 50-event replay) can reconstruct the evidence trail. A
/// consumer associates each event with its task via `project` + `issue_id` +
/// `task_id`, and ESCALATE ends in `pipeline_escalated`, never
/// `pipeline_completed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_workflow_publishes_lifecycle_events_to_project_event_bus() {
    let _guard = FACTORY_LOCK.lock().await;
    let repo = make_repo();
    let orchestrator = EchoClient::new(&["ACTION: ESCALATE\nHuman decision required."]);
    let coder = EchoClient::new(&[]);
    let reviewer = EchoClient::new(&[]);
    let idle = EchoClient::new(&[]);
    install_providers(vec![
        ("orchestrator", orchestrator.clone()),
        ("coder", coder.clone()),
        ("reviewer", reviewer.clone()),
        ("idle", idle.clone()),
    ]);

    // Real coding-team template so `is_controlled` sees a controlled team.
    let store = llm_harness_agent_team_studio::template_store::TemplateStore::new(None);
    let mut spec = store.load("coding-team").unwrap().spec;
    spec.id = "v06bus".into();
    spec.repo_path = Some(repo.to_string_lossy().to_string());
    spec.template = Some(llm_harness_agent_team_studio::team_spec::TemplateRef {
        template_id: "coding-team".into(),
        version: "6".into(),
    });
    for member in &mut spec.members {
        member.model = ModelRef::Explicit(
            match member.id.as_str() {
                "orchestrator" => "orchestrator",
                "coder" => "coder",
                "reviewer" => "reviewer",
                other => panic!("unexpected coding-team member {other}"),
            }
            .into(),
        );
    }

    let data_root = unique_dir("ct-v06-bus");
    let mut pm = TeamManager::new(&data_root);
    pm.create(spec, Some(repo.clone()), &settings(), "")
        .await
        .unwrap();

    let bus = Arc::new(llm_harness_agent_team_studio::events::EventBus::new());
    let issues_root = {
        let entry = pm.get("v06bus").unwrap();
        entry.issues.root().to_path_buf()
    };
    let issue_store = llm_harness_agent_team_studio::issue::IssueStore::new(&issues_root);
    let issue = issue_store
        .create("operator", "escalate for bus check", "no work expected")
        .unwrap();
    issue_store
        .set_status(&issue.id, IssueStatus::Confirmed)
        .unwrap();
    let confirmed = issue_store.load(&issue.id).unwrap().0;

    pm.get("v06bus")
        .unwrap()
        .start_workflow(confirmed.clone(), Some(bus.clone()))
        .unwrap();

    let events: Vec<serde_json::Value> = tokio::time::timeout(std::time::Duration::from_secs(8), async {
        loop {
            let parsed: Vec<serde_json::Value> = bus
                .replay()
                .iter()
                .filter_map(|line| serde_json::from_str(line).ok())
                .filter(|event: &serde_json::Value| event["type"] == "pipeline_escalated")
                .collect();
            if !parsed.is_empty() {
                break parsed;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("workflow did not reach the terminal event on the bus");

    assert_eq!(events.len(), 1);
    let escalated = &events[0];
    assert_eq!(escalated["project"], "v06bus");
    assert_eq!(escalated["issue_id"], confirmed.id);
    assert_eq!(escalated["reason"], "orchestrator escalated");
    // The full lifecycle is on the bus in order, project-tagged.
    let lifecycle: Vec<String> = bus
        .replay()
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["project"] == "v06bus")
        .filter(|event| event["issue_id"] == confirmed.id.as_str())
        .map(|event| event["type"].as_str().unwrap_or("?").to_string())
        .collect();
    assert_eq!(
        lifecycle,
        vec!["pipeline_started", "pipeline_escalated"],
        "ESCALATE must stay distinguishable from completion on the bus"
    );

    pm.shutdown("v06bus").await.unwrap();
}

/// R2 acceptance: an existing live subscriber (not replay) receives the
/// workflow lifecycle events published by the production construction path,
/// and revision rounds stay distinguishable on the bus.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_event_bus_subscriber_receives_workflow_lifecycle_events() {
    let _guard = FACTORY_LOCK.lock().await;
    let repo = make_repo();
    let orchestrator = EchoClient::new(&[
        "ACTION: DISPATCH coder\nRound-1 assignment: implement the feature.",
        "ACTION: DISPATCH coder\nRevision assignment: address round-1 findings.",
        "ACTION: FINISH\nRevised and verified.",
    ]);
    let coder = EchoClient::new(&[
        "round-1 evidence: feature implemented",
        "round-2 evidence: revision applied",
    ]);
    let reviewer = EchoClient::new(&[
        "round-1 review: one P1 finding",
        "round-2 review: no findings",
    ]);
    let idle = EchoClient::new(&[]);
    install_providers(vec![
        ("orchestrator", orchestrator.clone()),
        ("coder", coder.clone()),
        ("reviewer-robustness", reviewer.clone()),
        ("judge", idle),
    ]);

    // Production construction path: TeamManager::create builds the
    // controller; TeamEntry::start_workflow attaches the bus-backed sink.
    // The template marker makes the spec controlled, as in the existing
    // bus-bridge test.
    let mut spec = decision_spec("r2live", &repo);
    spec.template = Some(TemplateRef {
        template_id: "coding-team".into(),
        version: "6".into(),
    });
    let data_root = unique_dir("ct-r2-live-subscriber");
    let mut pm = TeamManager::new(&data_root);
    pm.create(spec, Some(repo.clone()), &settings(), "")
        .await
        .unwrap();
    let bus = Arc::new(llm_harness_agent_team_studio::events::EventBus::new());
    let mut rx = bus.subscribe();
    let (issues_root, workflow_controller) = {
        let entry = pm.get("r2live").unwrap();
        let issues_root = entry.issues.root().to_path_buf();
        let store = llm_harness_agent_team_studio::issue::IssueStore::new(&issues_root);
        let issue = store.create("operator", "revise twice", "two review rounds").unwrap();
        store.set_status(&issue.id, IssueStatus::Confirmed).unwrap();
        let confirmed = store.load(&issue.id).unwrap().0;
        entry
            .start_workflow(confirmed.clone(), Some(bus.clone()))
            .unwrap();
        (issues_root, entry.workflow.clone().unwrap())
    };
    let store = llm_harness_agent_team_studio::issue::IssueStore::new(&issues_root);
    let _ = &store;

    // Live subscriber observes lifecycle events in emission order.
    let deadline = std::time::Instant::now() + STAGE_BOUND;
    let mut started = false;
    let mut delegated = 0usize;
    #[allow(unused_assignments)] // read via `assert!(started && completed)` below
    let mut completed = false;
    loop {
        match rx.try_recv() {
            Ok(line) => {
                let event: serde_json::Value = serde_json::from_str(&line).unwrap();
                if event["project"] != "r2live" {
                    continue;
                }
                match event["type"].as_str() {
                    Some("pipeline_started") => started = true,
                    Some("task_delegated") => {
                        assert!(started, "task_delegated must follow pipeline_started");
                        delegated += 1;
                    }
                    Some("pipeline_completed") => {
                        assert!(delegated >= 4, "terminal must follow worker dispatches");
                        assert_eq!(event["reason"], "orchestrator finished");
                        completed = true;
                        break;
                    }
                    Some("pipeline_escalated") | Some("pipeline_failed") => {
                        panic!("FINISH workflow must not emit a failure event: {line}")
                    }
                    _ => {}
                }
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "live subscriber never saw pipeline_completed"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                panic!("event bus closed unexpectedly");
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                panic!("live subscriber must not lose workflow events");
            }
        }
    }
    assert!(started && completed);
    // Dispatches: 2 coder + 2 reviewer; revision is a re-DISPATCH of coder
    // emitted as revision_requested + task_delegated pair (see pipeline.rs).
    assert_eq!(delegated, 4, "2 coder dispatches + 2 reviewer dispatches");
    // Revision rounds are distinguishable on the bus: two snapshot rounds.
    let rounds: Vec<u64> = bus
        .replay()
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["type"] == "snapshot_collected")
        .map(|event| event["round"].as_u64().unwrap())
        .collect();
    assert_eq!(rounds, vec![1, 2], "review rounds must be distinguishable");

    let _ = workflow_controller;
    pm.shutdown("r2live").await.unwrap();
}

/// R2 acceptance: the production terminal path (workflow task inside
/// `start_workflow`) persists the terminal chat message exactly once and
/// projects it onto the bus exactly once. The abort path (project shutdown
/// mid-workflow) must not fabricate a success terminal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workflow_terminal_reaches_bus_once_and_cancel_creates_no_success() {
    let _guard = FACTORY_LOCK.lock().await;
    let repo = make_repo();
    let data_root = unique_dir("ct-r2-terminal-once");
    let mut pm = TeamManager::new(&data_root);
    let project_id = "r2terminal";

    // ── Completed workflow: one journaled terminal, one bus projection ──
    {
        let orchestrator = EchoClient::new(&["ACTION: FINISH\nSingle final answer."]);
        let coder = EchoClient::new(&[]);
        let reviewer = EchoClient::new(&[]);
        let idle = EchoClient::new(&[]);
        install_providers(vec![
            ("orchestrator", orchestrator.clone()),
            ("coder", coder.clone()),
            ("reviewer-robustness", reviewer.clone()),
            ("judge", idle),
        ]);
        pm.create(
            {
                let mut spec = decision_spec(project_id, &repo);
                spec.template = Some(TemplateRef {
                    template_id: "coding-team".into(),
                    version: "6".into(),
                });
                spec
            },
            Some(repo.clone()),
            &settings(),
            "",
        )
        .await
        .unwrap();
        let (bus, data_dir, issue_id) = {
            let entry = pm.get(project_id).unwrap();
            let data_dir = entry.data_dir().to_path_buf();
            let store = llm_harness_agent_team_studio::issue::IssueStore::new(entry.issues.root());
            let issue = store.create("operator", "finish fast", "one decision").unwrap();
            store.set_status(&issue.id, IssueStatus::Confirmed).unwrap();
            let confirmed = store.load(&issue.id).unwrap().0;
            let bus = Arc::new(llm_harness_agent_team_studio::events::EventBus::new());
            entry
                .start_workflow(confirmed.clone(), Some(bus.clone()))
                .unwrap();
            // The terminal row is the observable completion signal: the
            // workflow task journals it right after run() returns.
            tokio::time::timeout(STAGE_BOUND, async {
                loop {
                    if llm_harness_agent_team::ChatHistoryJournal::open(
                        data_dir.join("chat.sqlite3"),
                    )
                    .unwrap()
                    .group_history(50, None)
                    .unwrap()
                    .iter()
                    .any(|entry| {
                        entry
                            .message
                            .message_id
                            .as_deref()
                            .is_some_and(|m| m.ends_with(":terminal"))
                    })
                    {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("terminal chat message never landed");
            (bus, data_dir, confirmed.id)
        };
        let journal =
            llm_harness_agent_team::ChatHistoryJournal::open(data_dir.join("chat.sqlite3"))
                .unwrap();
        let history = journal.group_history(50, None).unwrap();
        assert_eq!(
            history.len(),
            1,
            "the terminal chat message must be the only workflow chat message: {history:?}"
        );
        assert_eq!(
            history[0].message.message_id.as_deref(),
            Some(format!("{issue_id}:terminal").as_str())
        );
        let terminal_events: Vec<serde_json::Value> = bus
            .replay()
            .iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .filter(|event: &serde_json::Value| {
                event["message_id"]
                    .as_str()
                    .is_some_and(|m| m.ends_with(":terminal"))
            })
            .collect();
        assert_eq!(
            terminal_events.len(),
            1,
            "terminal must be projected onto the bus exactly once"
        );
        assert_eq!(
            terminal_events[0]["message_id"],
            format!("{issue_id}:terminal")
        );
        pm.shutdown(project_id).await.unwrap();
    }

    // ── Aborted workflow: shutdown mid-decision must not emit a success ──
    {
        let repo = make_repo();
        // The orchestrator never answers: the run would park in `deciding`
        // until abort. Worker clients stay idle and must never run.
        let orchestrator = Arc::new(MockLlmClient::new(vec![]));
        let coder = EchoClient::new(&[]);
        install_providers(vec![
            ("orchestrator", orchestrator.clone() as Arc<dyn llm_harness_loop::LlmClient>),
            ("coder", coder.clone()),
            ("reviewer-robustness", EchoClient::new(&[])),
            ("judge", EchoClient::new(&[])),
        ]);
        let project_id = "r2abort";
        pm.create(
            {
                let mut spec = decision_spec(project_id, &repo);
                spec.template = Some(TemplateRef {
                    template_id: "coding-team".into(),
                    version: "6".into(),
                });
                spec
            },
            Some(repo.clone()),
            &settings(),
            "",
        )
        .await
        .unwrap();
        let bus = Arc::new(llm_harness_agent_team_studio::events::EventBus::new());
        {
            let entry = pm.get(project_id).unwrap();
            let store = llm_harness_agent_team_studio::issue::IssueStore::new(entry.issues.root());
            let issue = store.create("operator", "never finishes", "aborted").unwrap();
            store.set_status(&issue.id, IssueStatus::Confirmed).unwrap();
            let confirmed = store.load(&issue.id).unwrap().0;
            entry
                .start_workflow(confirmed.clone(), Some(bus.clone()))
                .unwrap();
            // Wait until the workflow is parked in the deciding phase.
            tokio::time::timeout(STAGE_BOUND, async {
                loop {
                    let phase = entry
                        .workflow
                        .as_ref()
                        .unwrap()
                        .state(&confirmed.id)
                        .map(|s| s.phase)
                        .unwrap_or_default();
                    if phase == "deciding" {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("workflow never started");
        }
        // Cancel: shutdown aborts the in-flight workflow task.
        pm.shutdown(project_id).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let success_events: Vec<serde_json::Value> = bus
            .replay()
            .iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .filter(|event: &serde_json::Value| event["type"] == "pipeline_completed")
            .collect();
        assert!(
            success_events_empty(&success_events),
            "an aborted workflow must not emit pipeline_completed"
        );
        let data_dir = data_root.join("teams").join(project_id);
        let journal =
            llm_harness_agent_team::ChatHistoryJournal::open(data_dir.join("chat.sqlite3"))
                .unwrap();
        let history = journal.group_history(50, None).unwrap();
        assert!(
            history.is_empty(),
            "an aborted workflow must not create a terminal chat message: {history:?}"
        );
    }
}

fn success_events_empty(events: &[serde_json::Value]) -> bool {
    events.is_empty()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decision_budget_reserves_post_work_orchestrator_decision() {
    let _guard = FACTORY_LOCK.lock().await;
    let repo = make_repo();
    let orchestrator = EchoClient::new(&[
        "ACTION: DISPATCH coder\nUse the last work-authorizing slot.",
        "ACTION: FINISH\nReviewed work is complete.",
    ]);
    let coder = EchoClient::new(&["coder result from the last work-authorizing slot"]);
    let reviewer = EchoClient::new(&["review result consumed by the reserved decision"]);
    let idle = EchoClient::new(&[]);
    install_providers(vec![
        ("orchestrator", orchestrator.clone()),
        ("coder", coder.clone()),
        ("reviewer-robustness", reviewer.clone()),
        ("judge", idle),
    ]);

    let data_root = unique_dir("ct-reserved-post-work-decision");
    let mut pm = TeamManager::new(&data_root);
    pm.create(
        decision_spec("reserve-post-work", &repo),
        Some(repo.clone()),
        &settings(),
        "",
    )
    .await
    .unwrap();
    let (ctx, members, issue) = {
        let entry = pm.get("reserve-post-work").unwrap();
        let issue = entry
            .issues
            .create("operator", "bounded work", "implement, review, then decide")
            .unwrap();
        let members = entry
            .spec
            .members
            .iter()
            .map(|m| m.id.clone())
            .collect::<Vec<_>>();
        (entry.team.social_context().unwrap().clone(), members, issue)
    };
    let controller = PipelineController::new("reserve-post-work", ctx, repo, &members, 2);
    let result = tokio::time::timeout(STAGE_BOUND, controller.run(&issue))
        .await
        .expect("bounded workflow timed out")
        .expect("reserved decision workflow failed");
    assert_eq!(result.as_deref(), Some("Reviewed work is complete."));
    let package = &controller
        .state(&issue.id)
        .unwrap()
        .terminal_user_outcome
        .unwrap()
        .body;
    assert!(package.contains("**Outcome:** FINISH"));
    assert!(package.contains("coder result from the last work-authorizing slot"));
    assert!(package.contains("review result consumed by the reserved decision"));
    assert!(package.contains("snapshot.json"));
    assert!(package.contains("Independently executed tests: unavailable"));
    assert_eq!(
        orchestrator
            .replies
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    assert_eq!(coder.replies.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        reviewer.replies.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    // Each decision run is one provider call: the correlated ACTION reply
    // seals the run, so the pre-seal text-only follow-up turn no longer
    // exists. The final (FINISH) decision is the second captured request.
    let final_decision_request = flatten_request(&orchestrator.captured_requests.lock()[1]);
    assert!(final_decision_request.contains("coder result from the last work-authorizing slot"));
    assert!(final_decision_request.contains("review result consumed by the reserved decision"));
    assert!(final_decision_request.contains("final decision slot is reserved"));
    let reviewer_request = flatten_request(&reviewer.captured_requests.lock()[0]);
    let reviewed_evidence_dir = reviewer_request
        .split_once("Snapshot evidence directory: ")
        .map(|(_, rest)| rest.lines().next().unwrap_or_default())
        .expect("reviewer receives the persisted snapshot path");
    assert!(
        final_decision_request.contains(&format!(
            "Snapshot evidence directory: {reviewed_evidence_dir}"
        )),
        "orchestrator must receive the same snapshot path reviewed by the reviewer"
    );
    pm.shutdown("reserve-post-work").await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn final_reserved_decision_rejects_dispatch_before_worker_runs() {
    let _guard = FACTORY_LOCK.lock().await;
    let repo = make_repo();
    let orchestrator = EchoClient::new(&["ACTION: DISPATCH coder\nToo late to start work."]);
    let coder = EchoClient::new(&[]);
    let reviewer = EchoClient::new(&[]);
    let idle = EchoClient::new(&[]);
    install_providers(vec![
        ("orchestrator", orchestrator),
        ("coder", coder.clone()),
        ("reviewer-robustness", reviewer.clone()),
        ("judge", idle),
    ]);

    let data_root = unique_dir("ct-reject-final-dispatch");
    let mut pm = TeamManager::new(&data_root);
    pm.create(
        decision_spec("reject-final-dispatch", &repo),
        Some(repo.clone()),
        &settings(),
        "",
    )
    .await
    .unwrap();
    let (ctx, members, issue) = {
        let entry = pm.get("reject-final-dispatch").unwrap();
        let issue = entry
            .issues
            .create("operator", "bounded work", "do not strand results")
            .unwrap();
        let members = entry
            .spec
            .members
            .iter()
            .map(|m| m.id.clone())
            .collect::<Vec<_>>();
        (entry.team.social_context().unwrap().clone(), members, issue)
    };
    let controller = PipelineController::new("reject-final-dispatch", ctx, repo, &members, 1);
    let err = tokio::time::timeout(STAGE_BOUND, controller.run(&issue))
        .await
        .expect("bounded workflow timed out")
        .unwrap_err();
    assert!(err.contains("final orchestrator decision slot is reserved"));
    assert_eq!(coder.replies.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(
        reviewer.replies.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    pm.shutdown("reject-final-dispatch").await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reviewer_blocked_and_error_are_non_success_evidence() {
    let _guard = FACTORY_LOCK.lock().await;
    let repo = make_repo();

    let orchestrator = EchoClient::new(&[
        "ACTION: DISPATCH coder\nImplement the feature.",
        "ACTION: DISPATCH coder\nRevise after the failed review attempt.",
        "ACTION: FINISH\nReported with review limitations.",
    ]);
    let coder = EchoClient::new(&["implementation one", "implementation two"]);
    let reviewer = EchoClient::new_with_kinds(
        &["review tool failed", "review prerequisite unavailable"],
        &["error", "blocked"],
    );
    let judge = EchoClient::new(&[]);
    install_providers(vec![
        ("orchestrator", orchestrator.clone()),
        ("coder", coder),
        ("reviewer-robustness", reviewer.clone()),
        ("judge", judge),
    ]);

    let data_root = unique_dir("ct-v0-review-non-success");
    let mut pm = TeamManager::new(&data_root);
    pm.create(
        decision_spec("v0-review-non-success", &repo),
        Some(repo.clone()),
        &settings(),
        "",
    )
    .await
    .unwrap();
    let (ctx, members, issues_root) = {
        let entry = pm.get("v0-review-non-success").unwrap();
        (
            entry.team.social_context().unwrap().clone(),
            entry
                .spec
                .members
                .iter()
                .map(|member| member.id.clone())
                .collect::<Vec<_>>(),
            entry.issues.root().to_path_buf(),
        )
    };
    let controller = PipelineController::new("v0-review-non-success", ctx, repo, &members, 3);
    let store = llm_harness_agent_team_studio::issue::IssueStore::new(&issues_root);
    let issue = store
        .create("operator", "implement", "then review")
        .unwrap();
    store.set_status(&issue.id, IssueStatus::Confirmed).unwrap();
    let issue = store.load(&issue.id).unwrap().0;

    let final_response = tokio::time::timeout(STAGE_BOUND, controller.run(&issue))
        .await
        .expect("workflow timed out")
        .expect("workflow failed");
    assert_eq!(
        final_response.as_deref(),
        Some("Reported with review limitations.")
    );

    let state = controller.state(&issue.id).unwrap();
    assert_eq!(state.phase, PipelinePhase::Done.as_str());
    assert!(
        state.reviewer_findings.is_empty(),
        "BLOCKED/ERROR must not be stored as successful reviewer findings"
    );
    let decisions = orchestrator
        .captured_requests
        .lock()
        .iter()
        .map(flatten_request)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(decisions.contains("Review outcome: ERROR"));
    assert!(decisions.contains("Review outcome: BLOCKED"));
    assert_eq!(
        reviewer.replies.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
}

/// An explicitly requested preliminary review remains possible, while each
/// later successful implementation still receives its mandatory review.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn v0_orchestrator_non_fixed_ordering() {
    let _guard = FACTORY_LOCK.lock().await;
    let repo = make_repo();

    let orchestrator_client = EchoClient::new(&[
        "ACTION: DISPATCH reviewer-robustness\nAssess the failure mode before any code is written.",
        "ACTION: DISPATCH coder\nImplement the fix from the reviewer's evidence.",
        "ACTION: DISPATCH coder\nHarden the fix per the remaining risk.",
        "ACTION: FINISH\nFinal response.",
    ]);
    let coder_client = EchoClient::new(&["evidence: fix applied", "evidence: hardening applied"]);
    let reviewer_client = EchoClient::new(&[
        "findings: race in module X (P1)",
        "implementation review: hardening needed",
        "revision review: no findings",
    ]);
    let judge_client = EchoClient::new(&[]);
    install_providers(vec![
        ("orchestrator", orchestrator_client.clone()),
        ("coder", coder_client.clone()),
        ("reviewer-robustness", reviewer_client.clone()),
        ("judge", judge_client.clone()),
    ]);

    let data_root = unique_dir("ct-v0-ordering");
    let mut pm = TeamManager::new(&data_root);
    pm.create(
        decision_spec("v0o", &repo),
        Some(repo.clone()),
        &settings(),
        "",
    )
    .await
    .unwrap();
    let (ctx, members, issues_root) = {
        let entry = pm.get("v0o").unwrap();
        let members: Vec<String> = entry.spec.members.iter().map(|m| m.id.clone()).collect();
        (
            entry.team.social_context().unwrap().clone(),
            members,
            entry.issues.root().to_path_buf(),
        )
    };
    let ledger = StageLedger::new();
    let sink_ledger = Arc::clone(&ledger);
    let controller = Arc::new(
        PipelineController::new("v0o", ctx.clone(), repo.clone(), &members, 6).with_sink(Arc::new(
            move |event: &str, payload: &serde_json::Value| {
                sink_ledger.record(event, payload);
            },
        )),
    );

    let store = llm_harness_agent_team_studio::issue::IssueStore::new(&issues_root);
    let issue = store
        .create("scout", "fix the race", "race reported in module X")
        .unwrap();
    store.set_status(&issue.id, IssueStatus::Confirmed).unwrap();
    let confirmed = store.load(&issue.id).unwrap().0;

    let run_handle = tokio::spawn({
        let controller = Arc::clone(&controller);
        let issue = confirmed.clone();
        async move { (*controller).run(&issue).await }
    });
    let run_outcome = match tokio::time::timeout(STAGE_BOUND, run_handle).await {
        Ok(joined) => joined,
        Err(_) => panic!(
            "workflow did not reach a terminal state within {STAGE_BOUND:?}\n{}",
            ledger_dump(&ledger)
        ),
    };
    run_outcome
        .unwrap_or_else(|e| panic!("workflow task panicked: {e}\n{}", ledger_dump(&ledger)))
        .unwrap_or_else(|e| panic!("workflow run failed: {e}\n{}", ledger_dump(&ledger)));

    assert_eq!(
        event_sequence(&ledger),
        vec![
            "pipeline_started".to_string(),
            "dispatch:reviewer-robustness".to_string(),
            "started:reviewer-robustness".to_string(),
            "done:reviewer-robustness".to_string(),
            "dispatch:coder".to_string(),
            "started:coder".to_string(),
            "done:coder".to_string(),
            "snapshot_collected".to_string(),
            "dispatch:reviewer-robustness".to_string(),
            "started:reviewer-robustness".to_string(),
            "done:reviewer-robustness".to_string(),
            "revise:coder".to_string(),
            "dispatch:coder".to_string(),
            "started:coder".to_string(),
            "done:coder".to_string(),
            "snapshot_collected".to_string(),
            "dispatch:reviewer-robustness".to_string(),
            "started:reviewer-robustness".to_string(),
            "done:reviewer-robustness".to_string(),
            "pipeline_completed".to_string(),
        ],
        "runtime must preserve preliminary review and mandatory post-coder review: {}",
        ledger_dump(&ledger),
    );
    let seq = std::sync::atomic::Ordering::SeqCst;
    assert_eq!(orchestrator_client.replies.load(seq), 4);
    assert_eq!(coder_client.replies.load(seq), 2);
    assert_eq!(reviewer_client.replies.load(seq), 3);
    assert_eq!(judge_client.replies.load(seq), 0);
}

/// Fresh coding-team v5 template end-to-end: the real LLM orchestrator
/// member exists, production ingress (`TeamEntry::start_workflow`) enters
/// the controlled workflow, and coder → snapshot → reviewer → orchestrator
/// FINISH reaches Done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_coding_team_v5_ingress_runs_through_orchestrator() {
    let _guard = FACTORY_LOCK.lock().await;
    let repo = make_repo();

    let orchestrator_client = EchoClient::new(&[
        "ACTION: DISPATCH coder\nFix the reported bug and return evidence.",
        "ACTION: FINISH\nFinal response.",
    ]);
    let coder_client = EchoClient::new(&["evidence: bug fixed with test"]);
    let reviewer_client = EchoClient::new(&["review evidence: no findings"]);
    let idle = Arc::new(MockLlmClient::new(vec![]));
    let idle_calls = idle.call_count.clone();
    install_providers(vec![
        ("orchestrator", orchestrator_client.clone()),
        ("coder", coder_client.clone()),
        ("reviewer", reviewer_client.clone()),
        ("idle", idle),
    ]);

    // Fresh template: real v5 coding-team members incl. the orchestrator;
    // models pinned to per-member mock clients, everything else idle.
    let store = llm_harness_agent_team_studio::template_store::TemplateStore::new(None);
    let mut spec = store.load("coding-team").unwrap().spec;
    spec.id = "v5fresh".into();
    spec.repo_path = Some(repo.to_string_lossy().to_string());
    spec.template = Some(TemplateRef {
        template_id: "coding-team".into(),
        version: "5".into(),
    });
    spec.template_baseline = Some(TemplateBaseline {
        version: "5".into(),
        members: spec.members.clone(),
    });
    for m in &mut spec.members {
        m.model = ModelRef::Explicit(
            match m.id.as_str() {
                "orchestrator" => "orchestrator",
                "coder" => "coder",
                "reviewer" => "reviewer",
                _ => "idle",
            }
            .into(),
        );
    }

    let data_root = unique_dir("ct-v5-fresh");
    let mut pm = TeamManager::new(&data_root);
    pm.create(spec, Some(repo.clone()), &settings(), "")
        .await
        .unwrap();
    let (controller, confirmed, orch_persona) = {
        let entry = pm.get("v5fresh").unwrap();
        assert!(
            entry.workflow.is_some(),
            "fresh v5 template team must be controlled"
        );
        let orch = entry
            .spec
            .members
            .iter()
            .find(|m| m.id == "orchestrator")
            .expect("fresh v5 template contains the orchestrator member");
        assert!(
            orch.persona.contains("ACTION: DISPATCH"),
            "orchestrator persona embeds the frozen ACTION contract"
        );
        let issue = entry
            .issues
            .create(
                "operator",
                "fix the login bug",
                "root cause unknown; provide evidence",
            )
            .unwrap();
        entry
            .issues
            .set_status(&issue.id, IssueStatus::Confirmed)
            .unwrap();
        let confirmed = entry.issues.load(&issue.id).unwrap().0;
        // Production ingress: the same path the confirm route takes.
        entry
            .start_workflow(confirmed.clone(), None)
            .expect("production ingress starts the controlled workflow");
        (
            entry.workflow.clone().unwrap(),
            confirmed,
            orch.persona.clone(),
        )
    };
    assert!(
        orch_persona.contains("ACTION: DISPATCH"),
        "expanded persona keeps the ACTION contract"
    );

    let deadline = tokio::time::Instant::now() + STAGE_BOUND;
    loop {
        let phase = controller.state(&confirmed.id).map(|s| s.phase);
        if phase.as_deref() == Some(PipelinePhase::Done.as_str()) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "fresh-template workflow did not finish in time; last phase {phase:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let seq = std::sync::atomic::Ordering::SeqCst;
    assert_eq!(orchestrator_client.replies.load(seq), 2);
    assert_eq!(coder_client.replies.load(seq), 1);
    assert_eq!(reviewer_client.replies.load(seq), 1);
    assert_eq!(idle_calls.load(seq), 0, "no unrelated member may run");
    pm.shutdown("v5fresh").await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn controlled_workers_work_before_terminal_report_and_progress_cannot_advance() {
    let _guard = FACTORY_LOCK.lock().await;
    let repo = make_repo();
    std::fs::write(repo.join("task.txt"), "inspect me").unwrap();

    let orchestrator = EchoClient::new(&[
        "ACTION: DISPATCH coder\nImplement and verify the requested change.",
        "ACTION: FINISH\nImplementation and review complete.",
    ]);
    let coder = WorkThenReportClient::new(WorkerLifecycleScript::Implement);
    let reviewer = WorkThenReportClient::new(WorkerLifecycleScript::Review);
    install_providers(vec![
        ("life-orchestrator", orchestrator.clone()),
        ("life-coder", coder.clone()),
        ("life-reviewer", reviewer.clone()),
    ]);

    let store = llm_harness_agent_team_studio::template_store::TemplateStore::new(None);
    let mut spec = store.load("coding-team").unwrap().spec;
    spec.id = "worker-life".into();
    spec.repo_path = Some(repo.to_string_lossy().to_string());
    spec.template = Some(TemplateRef {
        template_id: "coding-team".into(),
        version: "6".into(),
    });
    spec.template_baseline = Some(TemplateBaseline {
        version: "6".into(),
        members: spec.members.clone(),
    });
    for member in &mut spec.members {
        member.model = ModelRef::Explicit(
            match member.id.as_str() {
                "orchestrator" => "life-orchestrator",
                "coder" => "life-coder",
                "reviewer" => "life-reviewer",
                other => panic!("unexpected coding-team member {other}"),
            }
            .into(),
        );
    }

    let data_root = unique_dir("ct-worker-lifecycle");
    let mut pm = TeamManager::new(&data_root);
    pm.create(spec, Some(repo), &settings(), "").await.unwrap();
    let (controller, issue) = {
        let entry = pm.get("worker-life").unwrap();
        let issue = entry
            .issues
            .create("operator", "implement", "implement and review")
            .unwrap();
        entry
            .issues
            .set_status(&issue.id, IssueStatus::Confirmed)
            .unwrap();
        let issue = entry.issues.load(&issue.id).unwrap().0;
        entry.start_workflow(issue.clone(), None).unwrap();
        (entry.workflow.clone().unwrap(), issue)
    };

    tokio::time::timeout(STAGE_BOUND, async {
        while coder.calls.load(std::sync::atomic::Ordering::SeqCst) < 3 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("coder did not reach the post-progress boundary");

    assert_eq!(
        reviewer.calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a correlated acknowledgment/progress message must not dispatch reviewer"
    );
    assert_eq!(
        orchestrator
            .replies
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "progress must not request the next orchestrator decision"
    );
    assert_eq!(
        coder.choices.lock()[0],
        "Auto",
        "coder must retain normal tools on its first provider turn"
    );

    coder.resume_after_progress.notify_one();
    tokio::time::timeout(STAGE_BOUND, async {
        loop {
            if controller
                .state(&issue.id)
                .is_some_and(|state| state.phase == PipelinePhase::Done.as_str())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("valid worker and reviewer results did not complete the workflow");

    assert_eq!(reviewer.choices.lock()[0], "Auto");
    assert_eq!(
        format!(
            "{:?}",
            orchestrator.captured_requests.lock()[0].tool_choice()
        ),
        "Specific(\"send_message\")",
        "orchestrator decisions remain first-turn send_message calls"
    );
    assert_eq!(
        orchestrator
            .replies
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    assert_eq!(coder.calls.load(std::sync::atomic::Ordering::SeqCst), 4);
    assert_eq!(reviewer.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    pm.shutdown("worker-life").await.unwrap();
}

// ---------------------------------------------------------------------------
// Barebones V0 live acceptance (three agents, controlled pipeline)
// ---------------------------------------------------------------------------

/// Barebones V0 live acceptance, current build: the real builtin
/// coding-team template (exactly orchestrator/coder/reviewer), the real
/// production workflow ingress (`TeamEntry::start_workflow` — the same path
/// the `/api/team/chat` handler calls), real provider execution, and real
/// durable evidence.
///
/// Evidence collected (not fabricated):
/// - workflow terminal state (FINISH or ESCALATE, both are acceptance-valid
///   publications; the report distinguishes them),
/// - persisted Human Review Package in `chat.sqlite3` (`<issue>:terminal`,
///   published exactly once),
/// - historical Snapshot manifests + captured files r1 (and r2 when the run
///   exercises a real revision cycle),
/// - lifecycle events observed by a subscriber attached BEFORE the workflow
///   starts (immune to the 50-event replay truncation), correlated by
///   issue_id / task_id / review_round / snapshot_id,
/// - worker→operator isolation (no direct worker-to-operator chat rows).
///
/// Revision coverage: the objective asks for two increments, giving the real
/// reviewer a legitimate revision opportunity. If the model legitimately
/// FINISHes after one reviewed round, that is NOT faked here — the run is
/// reported as partial revision evidence and a separate run covers revision.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires live LLM endpoint (10.0.20.110:3001), ANTHROPIC_API_KEY, and local git; run with --ignored"]
async fn live_barebones_v0_three_agent_acceptance() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .unwrap_or_else(|_| panic!("set ANTHROPIC_API_KEY to the internal gateway token"));
    let base_url = "http://10.0.20.110:3001";
    let model = "moonshotai/kimi-k2.6";

    // Real git repo: the workflow's git_ignore_aware snapshot capture and
    // HEAD diff both require it.
    let repo = unique_dir("ct-live-v0-repo");
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
    std::fs::write(repo.join("README.md"), "# Barebones V0 acceptance\n").unwrap();
    git(&["add", "README.md"]);
    git(&["commit", "-m", "initial"]);

    // 1. Current builtin coding-team template, models pinned to the live tier.
    let mut spec = llm_harness_agent_team_studio::template_store::TemplateStore::new(None)
        .load(llm_harness_agent_team_studio::template_store::BUILTIN_CODING_TEAM_ID)
        .unwrap()
        .spec;
    let member_ids: Vec<&str> = spec.members.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        member_ids,
        ["orchestrator", "coder", "reviewer"],
        "Barebones V0 is exactly three agents; PipelineController is not a member"
    );
    for member in &mut spec.members {
        member.model = llm_harness_agent_team_studio::team_spec::ModelRef::Explicit(model.into());
    }

    let project_id = "coding-team";
    let data_root = unique_dir("ct-live-v0-data");
    let mut pm = TeamManager::new(&data_root);
    let settings = llm_harness_agent_team_studio::settings::Settings {
        models: tier(),
        scout_interval_secs: 0,
        base_url: base_url.into(),
    };
    pm.create(spec, Some(repo.clone()), &settings, &api_key)
        .await
        .unwrap();

    // 2. Event capture BEFORE the workflow starts: a live subscriber sees
    // every lifecycle event; the bus's 50-event replay buffer never truncates
    // the trail under observation.
    let bus = Arc::new(llm_harness_agent_team_studio::events::EventBus::new());
    let mut rx = bus.subscribe();

    // 3. Production ingress: start_workflow with a chat-shaped confirmed
    // issue — the same call the POST /api/team/chat handler makes.
    // The objective gives the reviewer a genuine revision opportunity
    // (two explicit increments), without scripting any verdict.
    let (controller, issue_id, data_dir) = {
        let entry = pm.get(project_id).unwrap();
        assert!(
            entry.workflow.is_some(),
            "coding-team template team must be controlled"
        );
        let issue = llm_harness_agent_team_studio::issue::Issue {
            id: format!("chat-{}", uuid::Uuid::new_v4()),
            status: IssueStatus::Confirmed,
            source: "operator".into(),
            created: chrono::Utc::now().to_rfc3339(),
            title: "Operator request".into(),
            body: String::from(
                "Two-increment task on this fresh repo.\n\
                 Increment 1: create `notes.py` implementing `add_note(text, path=\"notes.json\")` \
                 that appends to a JSON list and returns the new integer id, plus `test_notes.py` \
                 covering it; run the tests and report the results as evidence.\n\
                 Increment 2: after the first increment passes review, add `delete_note(note_id, path)` \
                 that never reuses ids, extend the tests, and rerun them.\n\
                 Each increment: implement, run the tests, then finish with the test results.\n\
                 The reviewer must check both increments before the final decision.",
            ),
        };
        let controller = entry.workflow.clone().unwrap();
        entry
            .start_workflow(issue.clone(), Some(bus.clone()))
            .expect("production ingress starts the workflow");
        (
            controller,
            issue.id.clone(),
            entry.data_dir().to_path_buf(),
        )
    };

    // 4. Observe the workflow's ACTUAL terminal state (bounded by the
    // production per-step budgets: up to 12 decision steps, 180s decisions,
    // 1800s worker executions).
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3600);
    let terminal_phase: String = loop {
        if let Some(state) = controller.state(&issue_id)
            && state.terminal_user_outcome.is_some()
        {
            break state.phase;
        }
        // Drain the live subscriber concurrently so the broadcast channel
        // never lags (a lagged subscriber would lose lifecycle evidence).
        while let Ok(line) = rx.try_recv() {
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(
                event["project"], project_id,
                "the bus must only carry this project's events: {line}"
            );
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "workflow did not reach a terminal state within 3600s; last state {:?}",
            controller.state(&issue_id)
        );
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    };
    let finished = terminal_phase == PipelinePhase::Done.as_str();

    let journal = llm_harness_agent_team::ChatHistoryJournal::open(data_dir.join("chat.sqlite3"))
        .unwrap();

    // 5. Durable terminal package: published exactly once under the
    // workflow-wide identity, with an explicit FINISH or ESCALATE outcome.
    let group = journal.group_history(50, None).unwrap();
    let terminals: Vec<_> = group
        .iter()
        .filter(|entry| {
            entry
                .message
                .message_id
                .as_deref()
                .is_some_and(|m| m == format!("{issue_id}:terminal"))
        })
        .collect();
    assert_eq!(
        terminals.len(),
        1,
        "the terminal review package must be published exactly once"
    );
    let package = &terminals[0].message.body;
    assert_eq!(
        finished,
        package.contains("**Outcome:** FINISH"),
        "package outcome must match the terminal phase ({terminal_phase})"
    );
    assert!(
        package.contains("## Human review package"),
        "the persisted terminal must be the structured review package: {package}"
    );
    assert!(
        package.contains("Evidence references"),
        "package must bind evidence references: {package}"
    );
    assert_eq!(
        group
            .iter()
            .filter(|entry| {
                entry
                    .message
                    .message_id
                    .as_deref()
                    .is_some_and(|m| m.ends_with(":terminal"))
            })
            .count(),
        1,
        "no other workflow may have published a terminal into this journal"
    );

    // 6. Worker isolation: no worker messaged the operator directly. Only
    // the orchestrator or the workflow-status identity may address the user.
    for member in ["coder", "reviewer"] {
        let direct_to_operator = journal
            .private_history(
                &AgentId::new(member),
                &AgentId::new("operator"),
                50,
                None,
            )
            .unwrap();
        assert!(
            direct_to_operator.is_empty(),
            "{member} must never converse directly with the operator: {direct_to_operator:?}"
        );
    }

    // 7. Durable Snapshot records: at least r1 exists with manifest + files;
    // r2 (and revision_requested) are required only when the run exercised
    // a real revision cycle. The snapshot id `<issue>#r<round>#<hash>` binds
    // the manifest to the workflow issue; the run directory is the only one
    // the workflow created.
    let snapshot_root = repo.join(".pipeline").join("snapshots");
    let mut runs = std::fs::read_dir(&snapshot_root)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    runs.sort();
    assert_eq!(
        runs.len(),
        1,
        "exactly one workflow run may have captured snapshots: {runs:?}"
    );
    let run_dir = snapshot_root.join(&runs[0]);
    let round_dirs: Vec<String> = std::fs::read_dir(&run_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with('r'))
        .collect();
    assert!(
        round_dirs.contains(&"r1".to_string()),
        "round-1 snapshot must be preserved: {round_dirs:?}"
    );
    for round in &round_dirs {
        let manifest = run_dir.join(round).join("snapshot.json");
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        assert_eq!(
            manifest["issue_id"], issue_id,
            "snapshot manifest must reference the workflow issue"
        );
        assert_eq!(
            manifest["run_id"].as_str().map(str::to_string).as_deref(),
            Some(runs[0].as_str()),
            "manifest run_id must match its directory"
        );
        let captured = std::fs::read_dir(run_dir.join(round).join("files"))
            .unwrap()
            .count();
        assert!(
            captured > 0,
            "{round} must have captured files, not just a manifest"
        );
    }
    let revised = round_dirs.contains(&"r2".to_string());

    // 8. Event correlation: every lifecycle event belongs to this workflow
    // and carries the right task/round/artifact identities.
    let mut started = false;
    let mut delegated: Vec<String> = Vec::new();
    let mut worker_started = 0usize;
    let mut worker_completed = 0usize;
    let mut snapshot_rounds: Vec<u64> = Vec::new();
    let mut revision_requested = false;
    let mut terminal_event: Option<String> = None;
    loop {
        match rx.try_recv() {
            Ok(line) => {
                let event: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(
                    event["project"], project_id,
                    "all events must belong to this project: {line}"
                );
                match event["type"].as_str() {
                    Some("pipeline_started") => started = true,
                    Some("task_delegated") => {
                        assert!(started, "task_delegated must follow pipeline_started");
                        assert!(
                            event["task_id"].as_str().is_some_and(|t| t.starts_with("task-pipe-")),
                            "dispatch must carry the controller task id: {line}"
                        );
                        assert_eq!(
                            event["issue_id"], issue_id,
                            "dispatch must reference the workflow issue: {line}"
                        );
                        delegated.push(
                            event["to"].as_str().unwrap_or("?").to_string(),
                        );
                    }
                    Some("worker_started") => {
                        worker_started += 1;
                        assert_eq!(event["issue_id"], issue_id);
                    }
                    Some("worker_completed") => {
                        worker_completed += 1;
                        assert_eq!(event["issue_id"], issue_id);
                    }
                    Some("snapshot_collected") => {
                        assert_eq!(event["issue_id"], issue_id);
                        snapshot_rounds.push(event["round"].as_u64().unwrap());
                    }
                    Some("revision_requested") => {
                        revision_requested = true;
                        assert_eq!(
                            event["review_round"], 1,
                            "the first revision follows round 1: {line}"
                        );
                        assert_eq!(event["to"], "coder");
                    }
                    Some("pipeline_completed") | Some("pipeline_escalated") => {
                        assert!(
                            terminal_event.is_none(),
                            "the workflow must terminate exactly once: {line}"
                        );
                        assert_eq!(event["issue_id"], issue_id);
                        terminal_event =
                            Some(event["type"].as_str().unwrap().to_string());
                        break;
                    }
                    Some(other) => panic!("unexpected lifecycle event {other}: {line}"),
                    None => panic!("event without a type: {line}"),
                }
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the event stream ended before the terminal event"
                );
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                panic!("live subscriber must not lose workflow events (lost {n})")
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                panic!("event bus closed before the terminal event")
            }
        }
    }
    assert!(started, "pipeline_started must be observed");
    assert_eq!(
        terminal_event.as_deref(),
        if finished {
            Some("pipeline_completed")
        } else {
            Some("pipeline_escalated")
        },
        "terminal event must match the terminal phase"
    );
    assert_eq!(
        delegated.first().map(String::as_str),
        Some("coder"),
        "the first dispatch must be the coder (orchestrator consumes work last)"
    );
    assert!(
        delegated.iter().any(|to| to == "reviewer"),
        "the reviewer must be dispatched at least once"
    );
    assert_eq!(
        worker_started, worker_completed,
        "every started worker must report a terminal outcome"
    );
    assert_eq!(
        snapshot_rounds,
        round_dirs
            .iter()
            .filter_map(|r| r[1..].parse::<u64>().ok())
            .collect::<Vec<_>>(),
        "snapshot events must match the durable snapshot rounds"
    );
    assert_eq!(
        revision_requested, revised,
        "revision_requested must agree with the durable r2 snapshot"
    );

    println!(
        "Barebones V0 live acceptance: phase={terminal_phase} rounds={round_dirs:?} \
         revision={revised} dispatches={delegated:?} events: started={started} \
         started_workers={worker_started} completed_workers={worker_completed}"
    );

    pm.shutdown(project_id).await.unwrap();
}

/// Automatic retention runs only after the workflow's workers have quiesced;
/// the global settings policy reaches both capture and the cleanup event bus.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completed_workflow_automatically_retains_to_low_watermark() {
    use llm_harness_agent_team_studio::{
        pipeline::ArtifactSnapshot,
        snapshot_storage::{SnapshotPolicy, begin_run, complete_run},
    };
    let _guard = FACTORY_LOCK.lock().await;
    let workspace = tempfile::tempdir().unwrap();
    let repo = workspace.path().to_path_buf();
    std::fs::create_dir(repo.join(".git")).unwrap();
    std::fs::write(repo.join("model.blend"), b"synthetic model workspace bytes").unwrap();
    let before_runs: Vec<_> = (0..2)
        .map(|_| {
            let run = uuid::Uuid::new_v4();
            begin_run(&repo, run).unwrap();
            ArtifactSnapshot::collect(&repo, "prior-completed", run, 0).unwrap();
            complete_run(&repo, run).unwrap();
            run
        })
        .collect();
    let data_root = tempfile::tempdir().unwrap();
    let mut policy = SnapshotPolicy::default();
    policy.capture.max_total_bytes = 512 << 20;
    policy.retention.enabled = true;
    policy.retention.count_high = 3;
    policy.retention.count_low = 1;
    std::fs::write(
        data_root.path().join("settings.json"),
        serde_json::to_vec(&serde_json::json!({"snapshots": policy})).unwrap(),
    )
    .unwrap();
    let orchestrator = EchoClient::new(&["ACTION: ESCALATE\nNo implementation needed."]);
    let idle = EchoClient::new(&[]);
    install_providers(vec![
        ("orchestrator", orchestrator),
        ("coder", idle.clone()),
        ("reviewer-robustness", idle.clone()),
        ("judge", idle),
    ]);
    let mut spec = decision_spec("snapshot-retention-auto", &repo);
    spec.template = Some(TemplateRef {
        template_id: "coding-team".into(),
        version: "6".into(),
    });
    let mut pm = TeamManager::new(data_root.path());
    pm.create(spec, Some(repo.clone()), &settings(), "")
        .await
        .unwrap();
    let entry = pm.get("snapshot-retention-auto").unwrap();
    let store = llm_harness_agent_team_studio::issue::IssueStore::new(entry.issues.root());
    let issue = store
        .create("operator", "synthetic retention", "no work")
        .unwrap();
    store.set_status(&issue.id, IssueStatus::Confirmed).unwrap();
    let confirmed = store.load(&issue.id).unwrap().0;
    let bus = Arc::new(llm_harness_agent_team_studio::events::EventBus::new());
    entry.start_workflow(confirmed, Some(bus.clone())).unwrap();
    tokio::time::timeout(STAGE_BOUND, async {
        while entry.workflow_running() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let events: Vec<serde_json::Value> = bus
        .replay()
        .iter()
        .map(|raw| serde_json::from_str(raw).unwrap())
        .collect();
    let cleanup = events
        .iter()
        .find(|event| event["type"] == "snapshot_retention")
        .expect("automatic cleanup is observable");
    assert_eq!(cleanup["issue_id"], issue.id);
    assert_eq!(cleanup["report"]["before"]["snapshots"], 3);
    assert_eq!(cleanup["report"]["after"]["snapshots"], 1);
    for run in before_runs {
        assert!(
            !repo
                .join(".pipeline/snapshots")
                .join(run.to_string())
                .exists()
        );
    }
    let retained: Vec<_> = std::fs::read_dir(repo.join(".pipeline/snapshots"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(retained.len(), 1);
    let snapshot: ArtifactSnapshot =
        serde_json::from_slice(&std::fs::read(retained[0].join("r0/snapshot.json")).unwrap())
            .unwrap();
    assert_eq!(snapshot.capture_limits.max_total_bytes, 512 << 20);
    assert!(repo.join(".git").is_dir());
    assert_eq!(
        std::fs::read(repo.join("model.blend")).unwrap(),
        b"synthetic model workspace bytes"
    );
    pm.shutdown("snapshot-retention-auto").await.unwrap();
}
