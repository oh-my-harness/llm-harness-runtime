#![cfg(feature = "live-llm")]

//! Tests: skills loading, prompt templates, and model switch.
//!
//! Covers:
//! - `load_skills` from directories + Plugin installation + `skill_read` tool
//! - `load_prompt_templates` + `invoke_template` placeholder expansion
//! - `set_model` mid-conversation recording `ModelChange` session entries
//! - EchoTool invoked by the LLM across multiple turns

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_agent::HarnessBuilder;
use llm_harness_agent::{
    PromptTemplate, Skill, invoke_template, load_prompt_templates, load_skills,
    session::types::SessionEntryPayload,
};
use llm_harness_live_tests::{
    EventCapture, LiveProvider, SINGLE_TURN, make_harness_with_env, make_skill_dir, with_timeout,
};
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolResult};

// ── Plugin: registers skills + templates from directory ──────────────────
struct SkillsPlugin {
    skills: Vec<Skill>,
    templates: Vec<PromptTemplate>,
}

impl llm_harness_agent::Plugin for SkillsPlugin {
    fn name(&self) -> &str {
        "skills-plugin"
    }

    fn register_skills(&self, skills: &mut Vec<Skill>) {
        skills.extend(self.skills.clone());
    }

    fn register_templates(&self, templates: &mut Vec<PromptTemplate>) {
        templates.extend(self.templates.clone());
    }
}

// ── Echo tool ─────────────────────────────────────────────────────────────
struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echo back the provided message. Use this when the user asks you to echo something."
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        &SCHEMA
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let msg = args
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("(no message)");
            Ok(ToolResult::full(
                vec![DataBlock::text(msg)],
                serde_json::Value::Null,
                false,
            ))
        })
    }
}

static SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "message": { "type": "string", "description": "The message to echo back" }
        },
        "required": ["message"]
    })
});

impl llm_harness_agent::Plugin for EchoTool {
    fn name(&self) -> &str {
        "echo-plugin"
    }

    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        tools.push(Arc::new(EchoTool) as Arc<dyn Tool>);
    }
}

// ── Test 1: skills loaded and read via skill_read tool ───────────────────
#[tokio::test]
async fn skills_loaded_and_readable() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    // Build harness with echo tool installed; we need env for load_skills.
    let (harness, env, _inspector, tmp) =
        make_harness_with_env(entry, |b| b.install(&EchoTool)).await;

    // Create 3 skill directories under a temp skills dir.
    let skills_dir = tmp.path().join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();

    make_skill_dir(
        &skills_dir,
        "python-style",
        "Python code style guidelines",
        "## Python Style\n\n- Use 4 spaces for indentation\n- Line length <= 88 chars\n- Use type hints\n- Prefer f-strings over .format()",
    );
    make_skill_dir(
        &skills_dir,
        "git-workflow",
        "Git commit and branch workflow",
        "## Git Workflow\n\n1. Create a feature branch\n2. Make changes\n3. Run tests\n4. Commit with conventional commits\n5. Create a PR",
    );
    make_skill_dir(
        &skills_dir,
        "code-review",
        "Code review checklist",
        "## Code Review Checklist\n\n- Tests pass\n- No obvious security issues\n- Naming is clear\n- No dead code\n- Error handling is appropriate",
    );

    // Load skills from the temp directory.
    let (skills, diagnostics) = load_skills(env.as_ref(), std::slice::from_ref(&skills_dir)).await;
    assert_eq!(skills.len(), 3, "expected 3 skills loaded");
    assert!(
        diagnostics.is_empty(),
        "expected no diagnostics, got: {:?}",
        diagnostics
    );

    // Drop the initial harness and rebuild with skills installed via Plugin.
    drop(harness);

    let builder = HarnessBuilder::new(&entry.model)
        .provider("*", entry.client.clone())
        .max_tokens(1024)
        .install(&EchoTool)
        .install(&SkillsPlugin {
            skills: skills.clone(),
            templates: vec![],
        });

    let harness = builder.build(env).await.unwrap();

    // 3 turns: ask the LLM to read skills via skill_read.
    let prompts = [
        "Read the python-style skill and tell me the indentation rule.",
        "Read the git-workflow skill and summarize the workflow steps.",
        "Read the code-review skill and list the checklist items.",
    ];

    let mut saw_skill_read = false;
    for (i, prompt) in prompts.iter().enumerate() {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(SINGLE_TURN, harness.prompt(*prompt))
            .await
            .expect("prompt failed");
        cap.settle().await;

        let tools = cap.tool_calls();
        if tools.iter().any(|t| t == "skill_read") {
            saw_skill_read = true;
        }
        eprintln!("turn {:2}: tools={:?}", i + 1, tools);
    }

    assert!(
        saw_skill_read,
        "expected at least one skill_read tool call across 3 turns"
    );
}

// ── Test 2: templates expand placeholders ────────────────────────────────
#[tokio::test]
async fn templates_expand_placeholders() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    // We need env for load_prompt_templates; harness content is incidental.
    let (harness, env, _inspector, tmp) = make_harness_with_env(entry, |b| b).await;

    // Create 2 template files.
    let templates_dir = tmp.path().join("templates");
    std::fs::create_dir_all(&templates_dir).unwrap();
    std::fs::write(
        templates_dir.join("summarize.md"),
        "---\ndescription: Summarize a topic\n---\n\nPlease provide a concise summary of: $1\n\nFocus on the key points.",
    )
    .unwrap();
    std::fs::write(
        templates_dir.join("explain.md"),
        "---\ndescription: Explain a concept\n---\n\nExplain the following concept in simple terms: $1\n\nUse analogies if helpful.",
    )
    .unwrap();

    let (templates, _diag) =
        load_prompt_templates(env.as_ref(), std::slice::from_ref(&templates_dir)).await;
    assert_eq!(templates.len(), 2, "expected 2 templates loaded");

    // Invoke the summarize template with an argument and verify expansion.
    let summarize_template = templates
        .iter()
        .find(|t| t.name == "summarize")
        .expect("summarize template should exist");
    let expanded = invoke_template(summarize_template, &["quantum computing".to_string()]);
    assert!(
        expanded.contains("quantum computing"),
        "expanded template should contain the argument; got: {}",
        expanded
    );

    // Invoke the explain template with a different argument.
    let explain_template = templates
        .iter()
        .find(|t| t.name == "explain")
        .expect("explain template should exist");
    let expanded2 = invoke_template(explain_template, &["recursion".to_string()]);
    assert!(
        expanded2.contains("recursion"),
        "expanded template should contain the argument; got: {}",
        expanded2
    );

    // Use an expanded template as a prompt to confirm it drives the LLM.
    let cap = EventCapture::subscribe(&harness);
    with_timeout(SINGLE_TURN, harness.prompt(expanded))
        .await
        .expect("prompt failed");
    cap.settle().await;
    assert!(
        !cap.texts().is_empty(),
        "template-driven prompt should produce text"
    );
}

// ── Test 3: model switch records ModelChange entries ─────────────────────
#[tokio::test]
async fn model_switch_records_entry() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let (harness, _env, _inspector, _tmp) = make_harness_with_env(entry, |b| b).await;

    // 2 turns on the original model.
    let prompts1 = ["Tell me a short joke.", "What is 2 + 2?"];

    for (i, prompt) in prompts1.iter().enumerate() {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(SINGLE_TURN, harness.prompt(*prompt))
            .await
            .expect("prompt failed");
        cap.settle().await;
        eprintln!(
            "phase1 turn {:2}: {}",
            i + 1,
            cap.texts().chars().take(60).collect::<String>()
        );
    }

    // Switch to an alternate model name and back — demonstrates set_model API.
    // Both switches record ModelChange entries. We switch back to the real
    // model before prompting so the LLM can actually respond.
    let alt_model = format!("{}-alt", entry.model);
    harness
        .set_model(alt_model.clone(), None)
        .await
        .expect("set_model should succeed");
    harness
        .set_model(entry.model.clone(), None)
        .await
        .expect("set_model back should succeed");

    // 2 turns after the model switch — conversation continues with history.
    let prompts2 = ["What model are you?", "Tell me about machine learning."];

    for (i, prompt) in prompts2.iter().enumerate() {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(SINGLE_TURN, harness.prompt(*prompt))
            .await
            .expect("prompt failed");
        cap.settle().await;
        eprintln!(
            "phase2 turn {:2}: {}",
            i + 1,
            cap.texts().chars().take(60).collect::<String>()
        );
    }

    // Verify ModelChange entries in the session.
    let entries = harness.read_active_path().await.unwrap();
    let model_changes: Vec<_> = entries
        .iter()
        .filter(|e| matches!(e.payload, SessionEntryPayload::ModelChange { .. }))
        .collect();
    assert!(
        model_changes.len() >= 2,
        "expected at least 2 ModelChange entries, got {}",
        model_changes.len()
    );
}

// ── Test 4: echo tool called by LLM ──────────────────────────────────────
#[tokio::test]
async fn echo_tool_called_by_llm() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let (harness, _env, _inspector, _tmp) =
        make_harness_with_env(entry, |b| b.install(&EchoTool)).await;

    // 10 turns explicitly asking the LLM to use the echo tool.
    let prompts = [
        "Use the echo tool to echo 'Hello, World!'",
        "Please call the echo tool with the message 'Testing echo'.",
        "Echo the word 'banana' using the echo tool.",
    ];

    let mut echo_turns = 0;
    for (i, prompt) in prompts.iter().enumerate() {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(SINGLE_TURN, harness.prompt(*prompt))
            .await
            .expect("prompt failed");
        cap.settle().await;

        let tools = cap.tool_calls();
        if tools.iter().any(|t| t == "echo") {
            echo_turns += 1;
        }
        eprintln!("turn {:2}: tools={:?}", i + 1, tools);
    }

    assert!(
        echo_turns >= 2,
        "expected echo tool to be called in at least 2 of 3 turns, got {}",
        echo_turns
    );
}
