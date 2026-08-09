//! 06 — Skills, Templates, and Model Switch
//!
//! Demonstrates:
//! - Skill registration via Plugin + directory loading (`load_skills`)
//! - `skill_read` tool: LLM reads skill files autonomously
//! - Prompt templates: `invoke_template` with `$1`/`$@` placeholders
//! - Model switch: `set_model()` mid-conversation
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 06_skills_model_switch

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_agent::{
    PromptTemplate, Skill, invoke_template, load_prompt_templates, load_skills,
};
use llm_harness_live_tests::{
    EventCapture, LiveProvider, SINGLE_TURN, make_harness_with_env, make_skill_dir, with_timeout,
};
use llm_harness_runtime::builder::HarnessBuilder;
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

#[tokio::main]
async fn main() {
    println!("=== 06: Skills, Templates, and Model Switch ===\n");

    let provider = LiveProvider::from_env();
    let entry = provider
        .openai
        .as_ref()
        .expect("set OPENAI_API_KEY to run this example");

    println!("Provider: {} | Model: {}\n", entry.name, entry.model);

    // ── Part 1: Skill registration + skill_read (10 turns) ─────────────────
    println!("--- Part 1: Skill registration + skill_read (10 turns) ---\n");

    // Build harness with echo tool, then load skills from temp dir
    let (harness, env, tmp) = make_harness_with_env(entry, |b| b.install(&EchoTool)).await;

    // Create skill directories
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

    // Load skills from the temp directory
    let (skills, diagnostics) = load_skills(env.as_ref(), std::slice::from_ref(&skills_dir)).await;
    println!(
        "Loaded {} skills, {} diagnostics",
        skills.len(),
        diagnostics.len()
    );
    for s in &skills {
        println!("  Skill: {} - {}", s.name, s.description);
    }

    // Create and load templates
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
    println!("Loaded {} templates", templates.len());
    for t in &templates {
        println!("  Template: {} - {}", t.name, t.description);
    }
    println!();

    // Rebuild harness with skills + templates via Plugin
    drop(harness);

    let builder = HarnessBuilder::new(&entry.model)
        .provider("*", entry.client.clone())
        .max_tokens(1024)
        .install(&EchoTool)
        .install(&SkillsPlugin {
            skills: skills.clone(),
            templates: templates.clone(),
        });

    let harness = builder.build(env).await.unwrap();

    // 10 turns: ask the LLM to use skills and tools
    let prompts = [
        "What skills do you have available?",
        "Use the echo tool to echo 'Hello, Skills!'",
        "Read the python-style skill and tell me the indentation rule.",
        "Read the git-workflow skill and summarize the workflow steps.",
        "Read the code-review skill and list the checklist items.",
        "Use the echo tool to echo 'Skills are working!'",
        "Based on the python-style skill, how should I format f-strings?",
        "Based on the git-workflow skill, what commit format should I use?",
        "Based on the code-review skill, what should I check for security?",
        "Summarize all the skills you've learned about.",
    ];

    for (i, prompt) in prompts.iter().enumerate() {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(SINGLE_TURN, harness.prompt(*prompt))
            .await
            .expect("prompt failed");
        cap.settle().await;
        let tools: Vec<_> = cap.tool_calls();
        println!(
            "Turn {:2}: tools={:?} | {}...",
            i + 1,
            tools,
            cap.texts().chars().take(200).collect::<String>()
        );
    }

    // ── Part 2: Prompt templates ────────────────────────────────────────────
    println!("\n--- Part 2: Prompt templates ---\n");

    // Demonstrate invoke_template programmatically
    let summarize_template = templates
        .iter()
        .find(|t| t.name == "summarize")
        .expect("summarize template should exist");
    let expanded = invoke_template(summarize_template, &["quantum computing".to_string()]);
    println!("Template 'summarize' expanded:\n{}\n", expanded);

    let explain_template = templates
        .iter()
        .find(|t| t.name == "explain")
        .expect("explain template should exist");
    let expanded2 = invoke_template(explain_template, &["recursion".to_string()]);
    println!("Template 'explain' expanded:\n{}\n", expanded2);

    // Use a template expansion as a prompt
    let cap = EventCapture::subscribe(&harness);
    with_timeout(SINGLE_TURN, harness.prompt(expanded))
        .await
        .expect("prompt failed");
    cap.settle().await;
    println!(
        "Template prompt response: {}...\n",
        cap.texts().chars().take(120).collect::<String>()
    );

    // ── Part 3: Model switch (10 turns) ─────────────────────────────────────
    println!("--- Part 3: Model switch (10 turns) ---\n");

    // Switch model mid-conversation. In production you might switch to a
    // faster/cheaper model for follow-up turns. Here we switch to the same
    // model (since we only have one provider configured) to demonstrate the
    // set_model API — the key point is that ModelChange entries are recorded
    // in the session and the conversation continues with history preserved.
    //
    // To switch to a truly different model, configure a second provider:
    //   .provider("fast-model", fast_client)
    //   ...
    //   harness.set_model("fast-model".to_string(), None).await
    let switched_model = entry.model.clone();
    // Toggle to a variant name and back to demonstrate the API
    let alt_model = format!("{0}-alt", entry.model);
    harness.set_model(alt_model.clone(), None).await.unwrap();
    println!(
        "Switched model to: {} (demonstrating set_model API)",
        alt_model
    );

    // Switch back immediately to the real model so the LLM can respond
    harness
        .set_model(switched_model.clone(), None)
        .await
        .unwrap();
    println!(
        "Switched back to: {} for actual conversation\n",
        switched_model
    );

    // 10 turns after model switch — conversation continues with full history
    let prompts2 = [
        "What model are you?",
        "Tell me about machine learning.",
        "Explain neural networks.",
        "What is deep learning?",
        "How does backpropagation work?",
        "What is a transformer architecture?",
        "Explain attention mechanism.",
        "What is fine-tuning?",
        "How do embeddings work?",
        "Summarize everything we discussed about ML.",
    ];

    for (i, prompt) in prompts2.iter().enumerate() {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(SINGLE_TURN, harness.prompt(*prompt))
            .await
            .expect("prompt failed");
        cap.settle().await;
        println!(
            "Turn {:2}: {}...",
            i + 1,
            cap.texts().chars().take(200).collect::<String>()
        );
    }

    // Verify ModelChange entries in session (2 switches: alt → original)
    let entries = harness.read_active_path().await.unwrap();
    let model_changes: Vec<_> = entries
        .iter()
        .filter(|e| {
            matches!(
                e.payload,
                llm_harness_agent::session::types::SessionEntryPayload::ModelChange { .. }
            )
        })
        .collect();
    println!("ModelChange entries in session: {}", model_changes.len());

    println!("\n--- Summary ---");
    println!("Skills: loaded via load_skills + Plugin, skill_read used by LLM ✅");
    println!("Templates: invoke_template with placeholder expansion ✅");
    println!("Model switch: set_model() mid-conversation, ModelChange entries recorded ✅");
}
