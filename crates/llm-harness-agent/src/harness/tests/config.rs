use super::*;

#[tokio::test]
async fn set_model_updates_state() {
    let h = make_harness(vec![]);
    h.set_model("claude-opus-4-7".into(), None).await.unwrap();
    assert_eq!(h.state().model, "claude-opus-4-7");
}

#[test]
fn set_system_prompt_sets_value() {
    let h = make_harness(vec![]);
    h.set_system_prompt(Some("new prompt".into()));
    assert_eq!(h.state().system_prompt.as_deref(), Some("new prompt"));
}

#[test]
fn set_system_prompt_clears_value() {
    let h = make_harness(vec![]);
    h.set_system_prompt(Some("initial".into()));
    h.set_system_prompt(None);
    assert_eq!(h.state().system_prompt, None);
}

#[test]
fn set_max_tokens_updates_inner() {
    let h = make_harness(vec![]);
    h.set_max_tokens(2048);
    assert_eq!(h.inner.lock().unwrap().max_tokens, 2048);
}

#[test]
fn set_max_tokens_persists_across_reads() {
    let h = make_harness(vec![]);
    h.set_max_tokens(4096);
    // Read twice — verifies the value sticks (not a one-shot injection).
    assert_eq!(h.inner.lock().unwrap().max_tokens, 4096);
    assert_eq!(h.inner.lock().unwrap().max_tokens, 4096);
}

#[tokio::test]
async fn skill_not_found_returns_error() {
    let h = make_harness(vec![]);
    let err = h.skill("nonexistent", None).await;
    assert!(matches!(err, Err(HarnessError::SkillNotFound(_))));
}

#[tokio::test]
async fn template_not_found_returns_error() {
    let h = make_harness(vec![]);
    let err = h.prompt_from_template("nonexistent", vec![]).await;
    assert!(matches!(
        err,
        Err(HarnessError::Template(TemplateError::NotFound(_)))
    ));
}

#[test]
#[should_panic(expected = "queue_capacity must be > 0")]
fn with_session_rejects_zero_queue_capacity() {
    let client: Arc<dyn LlmClient> = Arc::new(MockLlmClient::new(vec![]));
    let env: Arc<dyn ExecutionEnv> = Arc::new(NoOpEnv);
    let repo = InMemorySessionRepo::new();
    let storage = futures::executor::block_on(repo.create(CreateSessionOptions::default()))
        .expect("in-memory session creation cannot fail");
    let session = Session::new(storage);
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.queue_capacity = 0;
    let _ = AgentHarness::with_session(client, env, session, opts);
}

fn make_skill(name: &str, desc: &str) -> Skill {
    Skill {
        name: name.to_string(),
        label: None,
        description: desc.to_string(),
        source: std::path::PathBuf::from(format!("/skills/{name}/SKILL.md")),
        base_dir: std::path::PathBuf::from(format!("/skills/{name}")),
        disable_model_invocation: false,
    }
}

#[test]
fn add_skills_appends_without_replacing() {
    let h = make_harness(vec![]);
    assert_eq!(h.skills().len(), 0);

    h.add_skills(vec![make_skill("skill-a", "Does A")]);
    assert_eq!(h.skills().len(), 1);
    assert_eq!(h.skills()[0].name, "skill-a");

    h.add_skills(vec![make_skill("skill-b", "Does B")]);
    assert_eq!(h.skills().len(), 2);
    assert_eq!(h.skills()[0].name, "skill-a");
    assert_eq!(h.skills()[1].name, "skill-b");
}

#[test]
fn add_templates_appends_without_replacing() {
    let h = make_harness(vec![]);
    assert_eq!(h.templates().len(), 0);

    h.add_templates(vec![PromptTemplate {
        name: "tmpl-a".into(),
        description: "Template A".into(),
        content: "A $1".into(),
        source: std::path::PathBuf::from("/templates/a.md"),
    }]);
    assert_eq!(h.templates().len(), 1);
    assert_eq!(h.templates()[0].name, "tmpl-a");

    h.add_templates(vec![PromptTemplate {
        name: "tmpl-b".into(),
        description: "Template B".into(),
        content: "B $1".into(),
        source: std::path::PathBuf::from("/templates/b.md"),
    }]);
    assert_eq!(h.templates().len(), 2);
    assert_eq!(h.templates()[0].name, "tmpl-a");
    assert_eq!(h.templates()[1].name, "tmpl-b");
}
#[test]
fn add_skills_deduplicates_first_wins() {
    let h = make_harness(vec![]);
    h.add_skills(vec![make_skill("shared", "original")]);
    h.add_skills(vec![make_skill("shared", "should be dropped")]);
    assert_eq!(h.skills().len(), 1);
    assert_eq!(h.skills()[0].description, "original");
}

#[test]
fn add_skills_lazy_registers_skill_read_tool() {
    let h = make_harness(vec![]);
    // No skills at build time → no skill_read tool.
    assert!(
        !h.state().tools.iter().any(|t| t.name() == "skill_read"),
        "skill_read should not be registered when no skills at build time"
    );

    h.add_skills(vec![make_skill("lazy-skill", "Lazy loaded")]);

    // After add_skills, SkillReadTool should be registered.
    assert!(
        h.state().tools.iter().any(|t| t.name() == "skill_read"),
        "skill_read should be lazy-registered after add_skills"
    );
}

#[tokio::test]
async fn add_skills_lazy_injects_metadata_on_next_prompt() {
    use llm_harness_types::RunContext;
    use llm_harness_types::RunRequest;

    let h = make_harness(vec![MockResponse::text("ok")]);
    h.add_skills(vec![make_skill("lazy-skill", "Lazy loaded")]);

    // run_before_run_hook should inject <available-skills> via inline logic
    // (readable_skills is Some after add_skills).
    let run = RunContext::new(RunRequest::from_text("test"));
    let result = h.run_before_run_hook(&run, vec![]).await.unwrap();
    let has_metadata = result.iter().any(|m| match m {
        llm_harness_types::AgentMessage::User(u) => u.content.iter().any(|c| {
            matches!(c, llm_harness_types::ContentBlock::Text { text }
                    if text.contains("<available-skills>"))
        }),
        _ => false,
    });
    assert!(
        has_metadata,
        "inline injection should produce <available-skills> metadata after add_skills"
    );
}

#[test]
fn add_templates_deduplicates_first_wins() {
    let h = make_harness(vec![]);
    let tmpl = |name: &str, desc: &str| PromptTemplate {
        name: name.into(),
        description: desc.into(),
        content: format!("{name} $1"),
        source: std::path::PathBuf::from(format!("/templates/{name}.md")),
    };
    h.add_templates(vec![tmpl("shared", "original")]);
    h.add_templates(vec![tmpl("shared", "should be dropped")]);
    assert_eq!(h.templates().len(), 1);
    assert_eq!(h.templates()[0].description, "original");
}

#[test]
fn with_session_deduplicates_skills_in_opts() {
    let client: Arc<dyn LlmClient> = Arc::new(MockLlmClient::new(vec![]));
    let env: Arc<dyn ExecutionEnv> = Arc::new(NoOpEnv);
    let repo = InMemorySessionRepo::new();
    let storage = futures::executor::block_on(repo.create(CreateSessionOptions::default()))
        .expect("in-memory session creation cannot fail");
    let session = Session::new(storage);
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.skills = vec![make_skill("dup", "first"), make_skill("dup", "second")];
    opts.templates = vec![
        PromptTemplate {
            name: "t".into(),
            description: "first".into(),
            content: "x".into(),
            source: std::path::PathBuf::from("/t.md"),
        },
        PromptTemplate {
            name: "t".into(),
            description: "second".into(),
            content: "x".into(),
            source: std::path::PathBuf::from("/t2.md"),
        },
    ];
    let h = AgentHarness::with_session(client, env, session, opts);
    assert_eq!(h.skills().len(), 1);
    assert_eq!(h.skills()[0].description, "first");
    assert_eq!(h.templates().len(), 1);
    assert_eq!(h.templates()[0].description, "first");
}

#[test]
#[should_panic(expected = "is reserved by the framework")]
fn with_session_panics_when_user_tool_conflicts_with_harness_tool() {
    let client: Arc<dyn LlmClient> = Arc::new(MockLlmClient::new(vec![]));
    let env: Arc<dyn ExecutionEnv> = Arc::new(NoOpEnv);
    let repo = InMemorySessionRepo::new();
    let storage = futures::executor::block_on(repo.create(CreateSessionOptions::default()))
        .expect("in-memory session creation cannot fail");
    let session = Session::new(storage);
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.tools = vec![
        Arc::new(BuiltInLevelTool {
            name: "spawn_agent".into(),
        }),
        Arc::new(DummyTool::new("spawn_agent")), // User level, same name
    ];
    let _ = AgentHarness::with_session(client, env, session, opts);
}

#[tokio::test]
async fn set_tools_rejects_user_tool_conflicting_with_harness_tool() {
    let h = make_harness(vec![]);
    // First set a harness-level tool.
    h.set_tools(vec![Arc::new(BuiltInLevelTool {
        name: "my_harness_tool".into(),
    })])
    .await
    .unwrap();
    // Then try to set tools with a user-level tool of the same name.
    let err = h
        .set_tools(vec![
            Arc::new(BuiltInLevelTool {
                name: "my_harness_tool".into(),
            }),
            Arc::new(DummyTool::new("my_harness_tool")),
        ])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("reserved by the framework"));
}

#[tokio::test]
async fn set_tools_rejects_duplicate_user_tool_names() {
    let h = make_harness(vec![]);
    // Two user-level tools with the same name: error (duplicate).
    let err = h
        .set_tools(vec![
            Arc::new(DummyTool::new("dup")),
            Arc::new(DummyTool::new("dup")),
        ])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("duplicate tool name"));
}

#[test]
#[should_panic(expected = "is reserved by the framework")]
fn with_session_panics_when_user_tool_uses_reserved_name_alone() {
    // 孤立 User tool 名为 spawn_agent（单元素批次，无同批次 BuiltIn）。
    let client: Arc<dyn LlmClient> = Arc::new(MockLlmClient::new(vec![]));
    let env: Arc<dyn ExecutionEnv> = Arc::new(NoOpEnv);
    let repo = InMemorySessionRepo::new();
    let storage = futures::executor::block_on(repo.create(CreateSessionOptions::default()))
        .expect("in-memory session creation cannot fail");
    let session = Session::new(storage);
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.tools = vec![Arc::new(DummyTool::new("spawn_agent"))];
    let _ = AgentHarness::with_session(client, env, session, opts);
}

#[tokio::test]
async fn set_tools_rejects_user_tool_using_reserved_name_alone() {
    // 跨批次绕过尝试：set_tools replace 语义下，孤立 User tool
    // 名为 final_answer 也应被拒绝。
    let h = make_harness(vec![]);
    let err = h
        .set_tools(vec![Arc::new(DummyTool::new("final_answer"))])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("reserved by the framework"));
}

// ── LLM switching (provider / base URL / API key) ────────────────────────────

/// Two mock clients let us prove which client served a turn: each records its
/// own call count, and each has exactly one preset response (so a second call
/// against the same client would return the "(no more mock responses)" text).
fn make_switch_clients() -> (Arc<MockLlmClient>, Arc<MockLlmClient>) {
    (
        Arc::new(MockLlmClient::new(vec![MockResponse::text("from-a")])),
        Arc::new(MockLlmClient::new(vec![MockResponse::text("from-b")])),
    )
}

#[tokio::test]
async fn set_client_swaps_the_client_for_the_next_turn() {
    let (a, b) = make_switch_clients();
    let h = make_harness(vec![]);
    h.set_client(a.clone() as Arc<dyn LlmClient>);

    h.prompt("first").await.unwrap();
    h.wait_for_idle().await;
    assert_eq!(a.call_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(b.call_count.load(std::sync::atomic::Ordering::SeqCst), 0);

    h.set_client(b.clone() as Arc<dyn LlmClient>);
    h.prompt("second").await.unwrap();
    h.wait_for_idle().await;

    assert_eq!(
        a.call_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "old client must not be reused"
    );
    assert_eq!(
        b.call_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "new client must serve the next turn"
    );
    // The model carried by the request is unchanged: set_client is structural.
    assert_eq!(b.captured_requests.lock()[0].model(), "test-model");
}

#[tokio::test]
async fn switch_llm_swaps_client_and_model_together() {
    let (a, b) = make_switch_clients();
    let h = make_harness(vec![]);
    h.set_client(a.clone() as Arc<dyn LlmClient>);
    h.set_model("model-a".into(), None).await.unwrap();

    h.prompt("first").await.unwrap();
    h.wait_for_idle().await;
    assert_eq!(a.captured_requests.lock()[0].model(), "model-a");

    h.switch_llm(
        b.clone() as Arc<dyn LlmClient>,
        "model-b".into(),
        None,
        Some("anthropic".into()),
    )
    .await
    .unwrap();

    h.prompt("second").await.unwrap();
    h.wait_for_idle().await;

    assert_eq!(a.call_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(b.call_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(b.captured_requests.lock()[0].model(), "model-b");
    assert_eq!(h.state().model, "model-b");
}

#[tokio::test]
async fn switch_llm_records_model_change_with_provider() {
    let (_, b) = make_switch_clients();
    let h = make_harness(vec![]);

    h.switch_llm(
        b.clone() as Arc<dyn LlmClient>,
        "model-b".into(),
        None,
        Some("openai".into()),
    )
    .await
    .unwrap();

    let entries = h.session.read_active_path().await.unwrap();
    let changes: Vec<_> = entries
        .iter()
        .filter_map(|e| match &e.payload {
            SessionEntryPayload::ModelChange {
                to,
                provider,
                model_id,
            } => Some((to.clone(), provider.clone(), model_id.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(changes.len(), 1, "expected exactly one ModelChange entry");
    assert_eq!(changes[0].0, "model-b");
    assert_eq!(changes[0].1.as_deref(), Some("openai"));
    assert_eq!(changes[0].2, None);
}

/// A provider swap with an unchanged model id still has to be recorded: the
/// session log is the audit trail of which provider served which turn.
#[tokio::test]
async fn switch_llm_same_model_still_records_provider_change() {
    let (_, b) = make_switch_clients();
    let h = make_harness(vec![]);

    h.switch_llm(
        b.clone() as Arc<dyn LlmClient>,
        "test-model".into(),
        None,
        Some("anthropic".into()),
    )
    .await
    .unwrap();

    let entries = h.session.read_active_path().await.unwrap();
    let changes = entries
        .iter()
        .filter(|e| matches!(e.payload, SessionEntryPayload::ModelChange { .. }))
        .count();
    assert_eq!(changes, 1);
    assert_eq!(h.state().model, "test-model");
}
