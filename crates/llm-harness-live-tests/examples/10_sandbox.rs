//! 10 — Sandbox: OsEnvSandbox lifecycle, file I/O, shell execution, resource
//! limits (timeout), and running a harness inside the sandbox environment.
//!
//! Demonstrates:
//! - Creating and starting an `OsEnvSandbox` with a tempdir work directory
//! - File I/O roundtrip via `ExecutionEnv::write_file` / `read_text_file`
//! - Shell execution via `ExecutionEnv::execute_shell` with `ShellOptions`
//! - `ResourceLimits.timeout` killing a long-running command
//! - Running a live LLM harness whose tools execute inside the sandbox env
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 10_sandbox

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use llm_harness_live_tests::{
    EventCapture, LiveProvider, SINGLE_TURN, make_harness_with, with_timeout,
};
use llm_harness_platform::sandbox::{ResourceLimits, Sandbox, SandboxConfig};
use llm_harness_sandbox::OsEnvSandbox;
use llm_harness_types::{ExecutionEnv, ShellOptions, ShellOutput};
use tokio_util::sync::CancellationToken;

// ── Helper: build + start an OsEnvSandbox rooted at a tempdir ─────────────

/// ResourceLimits with no caps — equivalent to the `Default` the struct lacks.
fn no_limits() -> ResourceLimits {
    ResourceLimits {
        max_cpus: None,
        max_memory_mb: None,
        max_disk_mb: None,
        timeout: None,
    }
}

fn make_sandbox(tmp: &tempfile::TempDir, limits: ResourceLimits) -> OsEnvSandbox {
    let config = SandboxConfig {
        fs_allowlist: vec![tmp.path().to_path_buf()],
        fs_denylist: vec![],
        net_allowlist: vec![],
        resource_limits: limits,
        work_dir: Some(tmp.path().to_path_buf()),
    };
    OsEnvSandbox::new(config)
}

/// Default ShellOptions: no extra env, no timeout, no streaming callbacks.
fn default_opts() -> ShellOptions<'static> {
    ShellOptions {
        cwd: None,
        env: vec![],
        timeout: None,
        abort: CancellationToken::new(),
        on_stdout: None,
        on_stderr: None,
    }
}

/// Print a ShellOutput in a human-readable labeled block.
fn print_shell(label: &str, out: &ShellOutput) {
    println!("  {}", label);
    println!("    exit_code: {}", out.exit_code);
    if !out.stdout.is_empty() {
        // Trim trailing newline for cleaner display.
        println!("    stdout: {:?}", out.stdout.trim_end());
    } else {
        println!("    stdout: (empty)");
    }
    if !out.stderr.is_empty() {
        println!("    stderr: {:?}", out.stderr.trim_end());
    }
}

#[tokio::main]
async fn main() {
    println!("=== 10: Sandbox ===\n");

    // ── Part 1: File I/O roundtrip ─────────────────────────────────────────
    println!("--- Part 1: File I/O roundtrip ---\n");

    let tmp1 = tempfile::tempdir().unwrap();
    let sandbox1 = make_sandbox(&tmp1, no_limits());
    sandbox1.start().await.unwrap();
    println!(
        "Sandbox started. working_dir = {}",
        sandbox1.env().working_dir().display()
    );
    println!("is_running = {}", sandbox1.is_running());

    let env1: Arc<dyn ExecutionEnv> = sandbox1.env();
    let abort = CancellationToken::new();
    let file_path = Path::new("greeting.txt");
    let content = "Hello, sandbox!";

    env1.write_file(file_path, content.as_bytes(), abort.clone())
        .await
        .expect("write_file failed");
    println!("Wrote {:?} ({} bytes)", file_path, content.len());

    let read_back = env1
        .read_text_file(file_path, abort.clone())
        .await
        .expect("read_text_file failed");
    println!("Read back: {:?}", read_back);
    assert_eq!(read_back, content, "file I/O roundtrip mismatch");
    println!("Roundtrip verified: content matches.\n");

    // Also demonstrate list_dir + file_info on the file we just wrote.
    let entries = env1
        .list_dir(Path::new("."), abort.clone())
        .await
        .expect("list_dir failed");
    println!("list_dir('.') entries:");
    for e in &entries {
        let kind = if e.is_dir { "dir " } else { "file" };
        println!("  {} {:>6} B  {}", kind, e.size, e.path.display());
    }
    println!();

    // ── Part 2: Shell execution ────────────────────────────────────────────
    println!("--- Part 2: Shell execution ---\n");

    let tmp2 = tempfile::tempdir().unwrap();
    let sandbox2 = make_sandbox(&tmp2, no_limits());
    sandbox2.start().await.unwrap();
    let env2: Arc<dyn ExecutionEnv> = sandbox2.env();

    let out_echo = env2
        .execute_shell("echo 'shell output'", default_opts())
        .await
        .expect("execute_shell echo failed");
    print_shell("echo 'shell output'", &out_echo);
    println!();

    let out_ls = env2
        .execute_shell("ls -la", default_opts())
        .await
        .expect("execute_shell ls failed");
    print_shell("ls -la", &out_ls);
    println!();

    // Write a file via shell, then read it via ExecutionEnv to show the two
    // interfaces operate on the same filesystem view.
    let out_write = env2
        .execute_shell("echo 'from shell' > shell_file.txt", default_opts())
        .await
        .expect("execute_shell write failed");
    print_shell("echo 'from shell' > shell_file.txt", &out_write);

    let shell_file = env2
        .read_text_file(Path::new("shell_file.txt"), CancellationToken::new())
        .await
        .expect("read shell_file failed");
    println!("  read via ExecutionEnv: {:?}", shell_file.trim_end());
    println!();

    // ── Part 3: ResourceLimits (timeout) ───────────────────────────────────
    println!("--- Part 3: ResourceLimits (timeout) ---\n");

    let tmp3 = tempfile::tempdir().unwrap();
    let limits = ResourceLimits {
        max_cpus: None,
        max_memory_mb: None,
        max_disk_mb: None,
        timeout: Some(Duration::from_millis(100)),
    };
    let sandbox3 = make_sandbox(&tmp3, limits);
    sandbox3.start().await.unwrap();
    let env3: Arc<dyn ExecutionEnv> = sandbox3.env();

    // The per-call ShellOptions.timeout is what actually enforces the wall-clock
    // limit at execution time; mirror the sandbox config here.
    let mut opts = default_opts();
    opts.timeout = Some(Duration::from_millis(100));

    println!("Executing `sleep 5` with timeout = 100ms ...");
    let start = Instant::now();
    let result = env3.execute_shell("sleep 5", opts).await;
    let elapsed = start.elapsed();

    match &result {
        Ok(out) => {
            println!("  Command completed unexpectedly:");
            print_shell("sleep 5", out);
        }
        Err(e) => {
            println!("  Command failed as expected: {}", e);
        }
    }
    println!("  Elapsed: {:?}", elapsed);
    // Should return well before the 5s sleep finishes.
    assert!(
        elapsed < Duration::from_secs(3),
        "timeout did not interrupt the command in time: {:?}",
        elapsed
    );
    println!("Timeout verified: command was interrupted well before 5s.\n");

    // ── Part 4: Sandbox in harness context ─────────────────────────────────
    println!("--- Part 4: Sandbox in harness context ---\n");

    let provider = LiveProvider::from_env();
    let entry = match provider.openai.as_ref().or(provider.anthropic.as_ref()) {
        Some(e) => e,
        None => {
            eprintln!(
                "skipping Part 4: no provider configured (set OPENAI_API_KEY or ANTHROPIC_API_KEY)"
            );
            return;
        }
    };
    println!("Provider: {} | Model: {}", entry.name, entry.model);

    // make_harness_with internally creates + starts an OsEnvSandbox and wires
    // it into the harness so tool execution lands inside the sandbox env.
    let (harness, _tmp) = make_harness_with(entry, |b| b).await;
    println!("Harness built with sandbox-backed ExecutionEnv.");

    let capture = EventCapture::subscribe(&harness);
    let prompt = "Reply with exactly: sandbox ok";
    println!("Prompt: \"{}\"", prompt);

    with_timeout(SINGLE_TURN, harness.prompt(prompt))
        .await
        .expect("prompt failed");
    capture.settle().await;

    let text = capture.texts();
    println!("Response: {}", text);
    println!(
        "\nTokens: {} in / {} out",
        harness.usage().total_input_tokens,
        harness.usage().total_output_tokens
    );

    println!("\n=== 10: Sandbox complete ===");
}
