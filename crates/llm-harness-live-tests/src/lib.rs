//! Shared test harness for live LLM integration tests.
//!
//! Provides ProviderFactory, HarnessBuilder helpers, TieredTimeout,
//! Assertion helpers, EventCapture, and Fixture helpers used by both
//! `examples/` and `tests/`.
#[cfg(feature = "live-llm")]
mod inner {
    use std::env;
    use std::sync::Arc;

    use llm_harness_loop::{AnthropicProvider, LlmClient, OpenAIProvider};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use llm_harness_agent::AgentHarness;
    use llm_harness_runtime::builder::HarnessBuilder;
    use llm_harness_runtime::platform::sandbox::{ResourceLimits, Sandbox, SandboxConfig};
    use llm_harness_runtime_sandbox_os::OsEnvSandbox;

    /// A configured live LLM provider with its model name.
    pub struct LiveProviderEntry {
        pub name: String,  // "openai" or "anthropic"
        pub model: String, // model ID from env
        pub client: Arc<dyn LlmClient>,
    }

    /// Live providers constructed from environment variables.
    ///
    /// Reads `OPENAI_API_KEY` / `OPENAI_API_BASE` / `OPENAI_MODEL` and
    /// `ANTHROPIC_API_KEY` / `ANTHROPIC_API_BASE` / `ANTHROPIC_MODEL`.
    /// Missing keys → that provider is `None`.
    pub struct LiveProvider {
        pub openai: Option<LiveProviderEntry>,
        pub anthropic: Option<LiveProviderEntry>,
    }

    impl LiveProvider {
        /// Construct providers from environment variables.
        pub fn from_env() -> Self {
            let openai = env::var("OPENAI_API_KEY")
                .ok()
                .filter(|k| !k.is_empty())
                .map(|key| {
                    let base_url = env::var("OPENAI_API_BASE").unwrap_or_default();
                    let model = env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o".to_string());
                    let client: Arc<dyn LlmClient> = if base_url.is_empty() {
                        Arc::new(OpenAIProvider::builder(&key).build())
                    } else {
                        Arc::new(llm_harness_loop::openai_compatible(&key, &base_url))
                    };
                    LiveProviderEntry {
                        name: "openai".to_string(),
                        model,
                        client,
                    }
                });

            let anthropic = env::var("ANTHROPIC_API_KEY")
                .ok()
                .filter(|k| !k.is_empty())
                .map(|key| {
                    let base_url = env::var("ANTHROPIC_API_BASE").unwrap_or_default();
                    let model = env::var("ANTHROPIC_MODEL")
                        .unwrap_or_else(|_| "claude-sonnet-4-20250514".to_string());
                    let mut builder = AnthropicProvider::builder(&key);
                    if !base_url.is_empty() {
                        builder = builder.base_url(&base_url);
                    }
                    let client: Arc<dyn LlmClient> = Arc::new(builder.build());
                    LiveProviderEntry {
                        name: "anthropic".to_string(),
                        model,
                        client,
                    }
                });

            LiveProvider { openai, anthropic }
        }

        /// Return all configured providers for parameterized testing.
        pub fn all_providers(&self) -> Vec<&LiveProviderEntry> {
            let mut out = Vec::new();
            if let Some(e) = &self.openai {
                out.push(e);
            }
            if let Some(e) = &self.anthropic {
                out.push(e);
            }
            out
        }
    }

    // ── TieredTimeout ────────────────────────────────────────────────────────────

    pub const SMOKE_TIMEOUT: Duration = Duration::from_secs(30);
    pub const SINGLE_TURN: Duration = Duration::from_secs(60);
    pub const MULTI_TURN: Duration = Duration::from_secs(120);
    pub const COMPACTION: Duration = Duration::from_secs(120);
    pub const SPAWN: Duration = Duration::from_secs(120);

    /// Wrap a future with a timeout. Panics on timeout with a descriptive message.
    pub async fn with_timeout<T>(
        timeout: Duration,
        fut: impl std::future::Future<Output = T>,
    ) -> T {
        tokio::time::timeout(timeout, fut)
            .await
            .unwrap_or_else(|_| panic!("test timed out after {:?}", timeout))
    }

    // ── HarnessBuilder helper ────────────────────────────────────────────────────

    /// Create a tempdir-backed OsEnvSandbox.
    fn make_sandbox() -> (tempfile::TempDir, OsEnvSandbox) {
        let tmp = tempfile::tempdir().unwrap();
        let config = SandboxConfig {
            fs_allowlist: vec![tmp.path().to_path_buf()],
            fs_denylist: vec![],
            net_allowlist: vec![],
            resource_limits: ResourceLimits {
                max_cpus: None,
                max_memory_mb: None,
                max_disk_mb: None,
                timeout: None,
            },
            work_dir: Some(tmp.path().to_path_buf()),
        };
        let sandbox = OsEnvSandbox::new(config);
        (tmp, sandbox)
    }

    /// Build an `AgentHarness` with a real provider, tempdir sandbox, and InMemorySessionRepo.
    ///
    /// The returned `TempDir` must be held alive for the duration of the harness usage.
    pub async fn make_harness(entry: &LiveProviderEntry) -> (AgentHarness, tempfile::TempDir) {
        make_harness_with(entry, |b| b).await
    }

    /// Build an `AgentHarness` with a customization callback for injecting
    /// tools, hooks, plugins, etc.
    ///
    /// The callback receives the builder by value and returns it, enabling
    /// the chained builder API (`builder.tool(...).system_prompt(...)`).
    pub async fn make_harness_with(
        entry: &LiveProviderEntry,
        customize: impl FnOnce(HarnessBuilder) -> HarnessBuilder,
    ) -> (AgentHarness, tempfile::TempDir) {
        let (tmp, sandbox) = make_sandbox();
        sandbox.start().await.unwrap();
        let env: Arc<dyn llm_harness_types::ExecutionEnv> = sandbox.env();

        let builder = HarnessBuilder::new(&entry.model)
            .provider("*", entry.client.clone())
            .max_tokens(1024);

        let builder = customize(builder);

        let harness = builder.build(env).await.unwrap();
        (harness, tmp)
    }
    /// Like `make_harness_with`, but also returns the `ExecutionEnv` so callers
    /// can use it for `load_skills` / `load_prompt_templates` after build.
    pub async fn make_harness_with_env(
        entry: &LiveProviderEntry,
        customize: impl FnOnce(HarnessBuilder) -> HarnessBuilder,
    ) -> (
        AgentHarness,
        Arc<dyn llm_harness_types::ExecutionEnv>,
        tempfile::TempDir,
    ) {
        let (tmp, sandbox) = make_sandbox();
        sandbox.start().await.unwrap();
        let env: Arc<dyn llm_harness_types::ExecutionEnv> = sandbox.env();

        let builder = HarnessBuilder::new(&entry.model)
            .provider("*", entry.client.clone())
            .max_tokens(1024);

        let builder = customize(builder);

        let harness = builder.build(env.clone()).await.unwrap();
        (harness, env, tmp)
    }
    /// Build an `AgentHarness` with a Jsonl-backed session for persistence tests.
    ///
    /// The session is created in a subdirectory of the temp dir.
    /// The returned `TempDir` must be held alive.
    pub async fn make_harness_with_session(
        entry: &LiveProviderEntry,
        customize: impl FnOnce(HarnessBuilder) -> HarnessBuilder,
    ) -> (AgentHarness, tempfile::TempDir) {
        let (tmp, sandbox) = make_sandbox();
        sandbox.start().await.unwrap();
        let env: Arc<dyn llm_harness_types::ExecutionEnv> = sandbox.env();

        let session_dir = tmp.path().join("session");
        std::fs::create_dir_all(&session_dir).unwrap();
        use llm_harness_agent::SessionRepo;
        let repo = llm_harness_agent::JsonlSessionRepo::new(&session_dir);
        let storage = repo
            .create(llm_harness_agent::session::CreateSessionOptions::default())
            .await
            .unwrap();
        let session = llm_harness_agent::Session::new(storage);

        let builder = HarnessBuilder::new(&entry.model)
            .provider("*", entry.client.clone())
            .max_tokens(1024);

        let builder = customize(builder);

        let harness = builder.build_with_session(env, session).unwrap();
        (harness, tmp)
    }

    // ── EventCapture ─────────────────────────────────────────────────────────────

    use std::sync::Mutex;

    use llm_harness_agent::AgentHarnessEvent;
    use llm_harness_types::AgentEvent;

    /// Simplified event captured from the harness broadcast channel.
    #[derive(Debug, Clone)]
    pub enum CapturedEvent {
        TextDelta(String),
        ToolCallStart {
            tool_name: String,
        },
        ToolCallEnd {
            tool_name: String,
        },
        CompactionStart {
            tokens_before: usize,
        },
        CompactionEnd {
            tokens_before: usize,
            tokens_after: usize,
        },
        Settled,
        Aborted,
        Error(String),
    }

    /// Subscribes to `AgentHarnessEvent` broadcast and collects events for assertions.
    pub struct EventCapture {
        events: Arc<Mutex<Vec<CapturedEvent>>>,
    }

    impl EventCapture {
        /// Subscribe to the harness's event broadcast channel.
        ///
        /// Spawns a background task that drains events into an internal buffer.
        /// The task ends when the broadcast sender drops (harness dropped).
        pub fn subscribe(harness: &AgentHarness) -> Self {
            let events = Arc::new(Mutex::new(Vec::new()));
            let mut rx = harness.subscribe();
            let events_clone = events.clone();

            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(evt) => {
                            if let Some(captured) = convert_event(&evt) {
                                events_clone.lock().unwrap().push(captured);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    }
                }
            });

            EventCapture { events }
        }
        /// Wait briefly for the background task to drain remaining events.
        ///
        /// Call this after `prompt()` or `continue_run()` returns, before
        /// reading `events()` / `texts()` / `tool_calls()`. The harness's
        /// `prompt()` future resolves when the run completes, but the
        /// broadcast subscriber task may not have processed the final
        /// events (including `Settled`) yet.
        pub async fn settle(&self) {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        /// Return a snapshot of all captured events.
        pub fn events(&self) -> Vec<CapturedEvent> {
            self.events.lock().unwrap().clone()
        }

        /// Return all text deltas concatenated into a single string.
        pub fn texts(&self) -> String {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| match e {
                    CapturedEvent::TextDelta(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect()
        }

        /// Return the names of all tools that were called.
        pub fn tool_calls(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| match e {
                    CapturedEvent::ToolCallStart { tool_name } => Some(tool_name.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    /// Convert `AgentHarnessEvent` to `CapturedEvent`.
    ///
    /// Field names verified against source:
    /// - `AgentEvent::TextDelta { message_id, text }` → `text`
    /// - `AgentEvent::ToolCallStart { message_id, tool_use_id, name }` → `name`
    /// - `AgentEvent::ToolCallEnd { tool_use_id, args }` → no name field; use harness-level
    ///   `AgentHarnessEvent::ToolCallEnd { tool_name, .. }` instead for the tool name.
    fn convert_event(evt: &AgentHarnessEvent) -> Option<CapturedEvent> {
        match evt {
            AgentHarnessEvent::Agent(agent_event) => match agent_event {
                AgentEvent::TextDelta { text, .. } => Some(CapturedEvent::TextDelta(text.clone())),
                AgentEvent::ToolCallStart { name, .. } => Some(CapturedEvent::ToolCallStart {
                    tool_name: name.clone(),
                }),
                AgentEvent::Error(e) => Some(CapturedEvent::Error(format!("{:?}", e))),
                _ => None,
            },
            AgentHarnessEvent::ToolCallEnd { tool_name, .. } => Some(CapturedEvent::ToolCallEnd {
                tool_name: tool_name.clone(),
            }),
            AgentHarnessEvent::CompactionStart { estimated_tokens } => {
                Some(CapturedEvent::CompactionStart {
                    tokens_before: *estimated_tokens,
                })
            }
            AgentHarnessEvent::CompactionEnd { stats, .. } => Some(CapturedEvent::CompactionEnd {
                tokens_before: stats.as_ref().map(|s| s.tokens_before).unwrap_or(0),
                tokens_after: stats.as_ref().map(|s| s.tokens_after).unwrap_or(0),
            }),
            AgentHarnessEvent::Settled => Some(CapturedEvent::Settled),
            AgentHarnessEvent::Aborted => Some(CapturedEvent::Aborted),
            _ => None,
        }
    }

    // ── Assertion helpers ────────────────────────────────────────────────────────

    /// Assert that `haystack` contains `needle` (case-insensitive, trimmed).
    pub fn assert_text_contains(haystack: &str, needle: &str) {
        let h = haystack.to_lowercase();
        let n_lower = needle.to_lowercase();
        let n = n_lower.trim();
        assert!(
            h.contains(n),
            "expected text to contain {:?}, but got: {}",
            needle,
            haystack
        );
    }

    /// Assert that a tool with the given name was called (appears in captured events).
    pub fn assert_tool_was_called(events: &[CapturedEvent], tool_name: &str) {
        let found = events.iter().any(|e| match e {
            CapturedEvent::ToolCallStart { tool_name: n } => n == tool_name,
            _ => false,
        });
        assert!(
            found,
            "expected tool {:?} to be called, but it was not. Events: {:?}",
            tool_name, events
        );
    }

    /// Assert that a specific event kind was emitted.
    pub fn assert_event_emitted(events: &[CapturedEvent], kind: &CapturedEvent) {
        // Simple discriminant check
        let found = events
            .iter()
            .any(|e| std::mem::discriminant(e) == std::mem::discriminant(kind));
        assert!(
            found,
            "expected event {:?} to be emitted, but it was not",
            kind
        );
    }

    // ── Fixture helpers ──────────────────────────────────────────────────────────

    /// Create a temporary skill directory with a `SKILL.md` file.
    ///
    /// Returns the path to the skill directory (parent of `SKILL.md`).
    pub fn make_skill_dir(base: &Path, name: &str, description: &str, body: &str) -> PathBuf {
        let dir = base.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let content = format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n");
        std::fs::write(dir.join("SKILL.md"), content).unwrap();
        dir
    }
}
#[cfg(feature = "live-llm")]
pub use inner::{
    COMPACTION, CapturedEvent, EventCapture, LiveProvider, LiveProviderEntry, MULTI_TURN,
    SINGLE_TURN, SMOKE_TIMEOUT, SPAWN, assert_event_emitted, assert_text_contains,
    assert_tool_was_called, make_harness, make_harness_with, make_harness_with_env,
    make_harness_with_session, make_skill_dir, with_timeout,
};

/// Skip a test if no providers are configured.
#[macro_export]
macro_rules! skip_if_none {
    ($provider:expr) => {
        if $provider.is_none() {
            eprintln!("skipping test: provider not configured");
            return;
        }
    };
}
