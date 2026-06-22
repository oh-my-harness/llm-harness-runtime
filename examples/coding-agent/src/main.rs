//! CLI for the coding agent (Phase 1).
//!
//! Usage:
//!   coding-agent -p "your prompt"            # one-shot
//!   coding-agent "prompt"                     # positional arg (one-shot)
//!   echo "prompt" | coding-agent              # pipe
//!   coding-agent --interactive                # REPL
//!   coding-agent -i                           # REPL (short)
//!
//! Environment:
//!   ANTHROPIC_API_KEY          Required for Anthropic provider
//!   ANTHROPIC_API_BASE         Base URL for Anthropic (default: https://api.anthropic.com)
//!   ANTHROPIC_MODEL            Model name for Anthropic provider (default: claude-sonnet-4-6)
//!   OPENAI_API_KEY             API key for OpenAI-compatible provider
//!   OPENAI_API_BASE            Base URL (e.g. https://api.siliconflow.cn/v1)
//!   OPENAI_MODEL               Model name for OpenAI-compatible provider (default: gpt-4o)
//!   CODING_AGENT_MODEL         Override default model for any provider

use std::io::Read;
use std::sync::Arc;

use clap::Parser;
use llm_harness_agent::AgentHarness;
use llm_harness_agent::SessionRepo;
use llm_harness_agent::session::Session;
use llm_harness_agent::session::jsonl::JsonlSessionRepo;
use llm_harness_agent::session::types::CreateSessionOptions;
use llm_harness_runtime::HarnessBuilder;
use llm_harness_runtime_sandbox_os::OsEnv;
use llm_harness_types::{AgentError, HarnessError};

use coding_agent::plugin::CodingAgentPlugin;
use coding_agent::provider;
use coding_agent::ui;

const SESSION_DIR: &str = ".coding-agent/sessions";

#[derive(Parser)]
#[command(
    name = "coding-agent",
    version,
    about = "An LLM-powered coding assistant"
)]
struct Cli {
    /// Prompt text (one-shot mode, positional)
    prompt: Option<String>,

    /// Prompt text (one-shot mode, explicit flag)
    #[arg(short = 'p', long = "print")]
    print: Option<String>,

    /// Start interactive REPL
    #[arg(short = 'i', long = "interactive")]
    interactive: bool,
}

#[tokio::main]
async fn main() {
    let code = run().await.err().unwrap_or(0);
    std::process::exit(code);
}

async fn run() -> Result<(), i32> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    // ── Parse args ────────────────────────────────────────────────────────────────

    let cli = Cli::try_parse().map_err(|e| {
        // clap prints the error / help to stderr, exit with code 2
        e.print().expect("Error writing to stderr");
        2
    })?;

    // ── Detect mode: interactive vs one-shot ──────────────────────────────────────

    let interactive = cli.interactive
        || (atty::is(atty::Stream::Stdin) && cli.print.is_none() && cli.prompt.is_none());

    let prompt_opt = if interactive {
        None
    } else {
        Some(resolve_prompt(&cli).map_err(|msg| {
            eprintln!("Error: {msg}");
            1
        })?)
    };

    // ── Resolve provider ────────────────────────────────────────────────────────

    let resolved = provider::resolve_provider().ok_or(1)?;

    // ── Build harness ───────────────────────────────────────────────────────────

    let plugin = CodingAgentPlugin::new(&cwd);
    let env = Arc::new(OsEnv::new(&cwd));

    let session = create_session(&cwd, &resolved.model).await.map_err(|e| {
        eprintln!("Error creating session: {e}");
        1
    })?;

    let harness = Arc::new(
        HarnessBuilder::new(&resolved.model)
            .install(&plugin)
            .provider(&resolved.model, resolved.client)
            .build_with_session(env, session)
            .map_err(|e| {
                eprintln!("Error building agent: {e}");
                1
            })?,
    );

    // ── Wire Ctrl-C to abort the current turn instead of killing the process ──

    let h = harness.clone();
    tokio::spawn(async move {
        loop {
            tokio::signal::ctrl_c().await.ok();
            h.abort();
        }
    });

    // ── Run ────────────────────────────────────────────────────────────────────

    if interactive {
        run_repl(&harness).await
    } else {
        let prompt = prompt_opt.unwrap();
        run_one_shot(&harness, &prompt).await
    }
}

// ── One-shot mode ──────────────────────────────────────────────────────────────

async fn run_one_shot(harness: &Arc<AgentHarness>, prompt: &str) -> Result<(), i32> {
    let events = harness.subscribe();
    let printer = tokio::spawn(ui::render_one_shot(events));

    match harness.prompt(prompt).await {
        Err(HarnessError::Agent(AgentError::Aborted)) => {
            // Ctrl-C was pressed — abort is expected, not an error.
        }
        Err(e) => {
            eprintln!("\nError: {e}");
        }
        Ok(()) => {}
    }

    let _ = printer.await;
    println!();
    Ok(())
}

// ── Interactive REPL ─────────────────────────────────────────────────────────────

async fn run_repl(harness: &Arc<AgentHarness>) -> Result<(), i32> {
    use rustyline::DefaultEditor;
    use rustyline::error::ReadlineError;

    let history_path = dirs_next::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("coding-agent")
        .join("history");

    let mut editor = DefaultEditor::new().map_err(|e| {
        eprintln!("Failed to initialize line editor: {e}");
        1
    })?;

    if history_path.exists() {
        let _ = editor.load_history(&history_path);
    }

    loop {
        let line = match editor.readline("> ") {
            Ok(l) => l,
            Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => break,
            Err(e) => {
                eprintln!("Error reading input: {e}");
                let _ = save_history(&mut editor, &history_path);
                return Err(1);
            }
        };

        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line == "exit" || line == "quit" {
            break;
        }
        let _ = editor.add_history_entry(&line);

        if run_one_shot(harness, &line).await.is_err() {
            let _ = save_history(&mut editor, &history_path);
            return Err(1);
        }
    }

    let _ = save_history(&mut editor, &history_path);
    Ok(())
}

fn save_history(
    editor: &mut rustyline::DefaultEditor,
    path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    editor.save_history(path)?;
    Ok(())
}

// ── Arg resolution (one-shot) ─────────────────────────────────────────────────

fn resolve_prompt(cli: &Cli) -> Result<String, String> {
    // -p / --print takes precedence over positional
    if let Some(ref p) = cli.print {
        return Ok(p.clone());
    }
    if let Some(ref p) = cli.prompt {
        return Ok(p.clone());
    }
    // Stdin (non-TTY only)
    if !atty::is(atty::Stream::Stdin) {
        let mut buf = String::new();
        if std::io::stdin().read_to_string(&mut buf).is_ok() {
            let trimmed = buf.trim().to_string();
            if !trimmed.is_empty() {
                return Ok(trimmed);
            }
        }
    }
    Err("No prompt provided. Use -p \"prompt\", pass text as argument, or pipe via stdin.".into())
}

// ── Session persistence ──────────────────────────────────────────────────────────

/// Create a file-backed session in `.coding-agent/sessions/` under `cwd`.
async fn create_session(cwd: &std::path::Path, model: &str) -> Result<Session, String> {
    let session_dir = cwd.join(SESSION_DIR);
    std::fs::create_dir_all(&session_dir).map_err(|e| {
        format!(
            "Failed to create session dir {}: {e}",
            session_dir.display()
        )
    })?;

    let repo = JsonlSessionRepo::new(&session_dir);
    let storage = repo
        .create(CreateSessionOptions {
            name: None,
            initial_model: Some(model.to_string()),
            initial_thinking_level: None,
            initial_tools: vec![],
        })
        .await
        .map_err(|e| format!("Failed to create session: {e}"))?;

    Ok(Session::new(storage))
}
