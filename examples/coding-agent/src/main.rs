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
//!   OPENAI_API_KEY             API key for OpenAI-compatible provider
//!   OPENAI_API_BASE            Base URL (e.g. https://api.siliconflow.cn/v1)
//!   OPENAI_MODEL               Model name for OpenAI-compatible provider
//!   CODING_AGENT_MODEL         Override default model (else claude-sonnet-4-6)

use std::sync::Arc;

use clap::Parser;
use llm_adapter::anthropic::AnthropicProvider;
use llm_adapter::openai::OpenAIProvider;
use llm_harness_loop::LlmClient;
use llm_harness_runtime::HarnessBuilder;
use llm_harness_runtime_sandbox_os::OsEnv;

use coding_agent::output;
use coding_agent::plugin::CodingAgentPlugin;

const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

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
    let code = run().await;
    std::process::exit(code);
}

async fn run() -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    // ── Parse args ────────────────────────────────────────────────────────────────

    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            // clap prints the error / help to stderr, exit with code 2
            e.print().expect("Error writing to stderr");
            return 2;
        }
    };

    // ── Detect mode: interactive vs one-shot ──────────────────────────────────────

    let interactive = cli.interactive
        || (atty::is(atty::Stream::Stdin) && cli.print.is_none() && cli.prompt.is_none());

    let prompt_opt = if interactive {
        None
    } else {
        match resolve_prompt(&cli) {
            Ok(p) => Some(p),
            Err(msg) => {
                eprintln!("Error: {msg}");
                return 1;
            }
        }
    };

    // ── Resolve provider ────────────────────────────────────────────────────────

    let openai_key = std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    let openai_base = std::env::var("OPENAI_API_BASE")
        .ok()
        .filter(|s| !s.is_empty());

    let (client, default_model): (Arc<dyn LlmClient>, String) =
        if let (Some(key), Some(base)) = (openai_key, openai_base) {
            let base_trimmed = base.trim_end_matches('/').to_string();
            let (base_url, chat_path) = if base_trimmed.ends_with("/v1") {
                (base_trimmed, "/chat/completions".to_string())
            } else {
                (base_trimmed, "/v1/chat/completions".to_string())
            };
            let provider = OpenAIProvider::builder(key)
                .base_url(base_url)
                .chat_path(chat_path)
                .build();
            let model = std::env::var("OPENAI_MODEL")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "gpt-4o".to_string());
            (Arc::new(provider), model)
        } else {
            let api_key = match std::env::var("ANTHROPIC_API_KEY") {
                Ok(k) if !k.is_empty() => k,
                _ => {
                    eprintln!("Error: set ANTHROPIC_API_KEY or OPENAI_API_KEY + OPENAI_API_BASE");
                    return 1;
                }
            };
            (
                Arc::new(AnthropicProvider::builder(api_key).build()),
                DEFAULT_MODEL.to_string(),
            )
        };

    let model = std::env::var("CODING_AGENT_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or(default_model);

    // ── Build harness ───────────────────────────────────────────────────────────

    let plugin = CodingAgentPlugin::new(&cwd);
    let env = Arc::new(OsEnv::new(cwd));

    let harness = match HarnessBuilder::new(&model)
        .install(&plugin)
        .provider(&model, client)
        .build(env)
        .await
    {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Error building agent: {e}");
            return 1;
        }
    };

    // ── Run ────────────────────────────────────────────────────────────────────

    if interactive {
        run_repl(&harness).await
    } else {
        let prompt = prompt_opt.unwrap();
        run_one_shot(&harness, &prompt).await
    }
}

// ── One-shot mode ──────────────────────────────────────────────────────────────

async fn run_one_shot(harness: &llm_harness_agent::AgentHarness, prompt: &str) -> i32 {
    let events = harness.subscribe();
    let printer = tokio::spawn(output::render_one_shot(events));

    if let Err(e) = harness.prompt(prompt).await {
        eprintln!("\nError: {e}");
    }

    let _ = printer.await;
    println!();
    0
}

// ── Interactive REPL ─────────────────────────────────────────────────────────────

async fn run_repl(harness: &llm_harness_agent::AgentHarness) -> i32 {
    use rustyline::DefaultEditor;
    use rustyline::error::ReadlineError;

    let history_path = dirs_next::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("coding-agent")
        .join("history");

    let mut editor = match DefaultEditor::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Failed to initialize line editor: {e}");
            return 1;
        }
    };

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
                return 1;
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

        if run_one_shot(harness, &line).await != 0 {
            let _ = save_history(&mut editor, &history_path);
            return 1;
        }
    }

    let _ = save_history(&mut editor, &history_path);
    0
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

use std::io::Read;
