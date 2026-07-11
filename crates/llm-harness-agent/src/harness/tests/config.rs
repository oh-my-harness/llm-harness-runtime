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
        content: format!("{name} body"),
        source: std::path::PathBuf::from(format!("/skills/{name}/SKILL.md")),
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
