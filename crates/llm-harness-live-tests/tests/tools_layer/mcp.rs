//! Tests for MCP integration (Spec §7).
//!
//! MCP requires an external MCP server process. The test below discovers
//! tools from `npx -y @modelcontextprotocol/server-everything` and asserts
//! that at least one tool is exposed.
//!
//! Per Spec §9.1 (层内积累 / layer-internal accumulation), this test is
//! marked `#[ignore]` because it depends on an external MCP server binary
//! that may not be available in every environment. Run with:
//!
//! ```sh
//! cargo test -p llm-harness-live-tests --test tools_layer --features live-llm mcp_tool_discovery -- --ignored
//! ```

#![cfg(feature = "live-llm")]

use std::collections::HashMap;
use std::sync::Arc;

use llm_harness_live_tests::skip_if_none;
use llm_harness_live_tests::{LiveProvider, SINGLE_TURN, with_timeout};
use llm_harness_runtime_mcp::config::McpServerConfig;
use llm_harness_runtime_mcp::{McpManager, McpToolAdapter, McpToolSource};
use llm_harness_types::Tool;

// ── Tests ──────────────────────────────────────────────────────────────

/// MCP tools are discovered from a stdio MCP server.
///
/// This test connects to `npx -y @modelcontextprotocol/server-everything`,
/// discovers its tools via `McpToolSource::into_adapters()`, and asserts
/// that at least one tool adapter is produced. The adapters implement the
/// core `Tool` trait and would be registered on a harness in a full
/// integration scenario.
///
/// Marked `#[ignore]` because it requires `npx` and network access to
/// fetch the `@modelcontextprotocol/server-everything` package.
#[tokio::test]
#[ignore = "requires MCP server (npx @modelcontextprotocol/server-everything)"]
async fn mcp_tool_discovery() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let _entry = entry.unwrap();

    // ── Configure a stdio MCP server ───────────────────────────────────
    let manager = Arc::new(McpManager::new());

    let mut servers = HashMap::new();
    servers.insert(
        "everything".to_string(),
        McpServerConfig::Stdio {
            command: "npx".to_string(),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-everything".to_string(),
            ],
            env: HashMap::new(),
            cwd: None,
            timeout: None,
            version_policy: Default::default(),
            credentials: None,
        },
    );

    // Discover and connect — this spawns the server process and
    // initializes the MCP session. STARTUP_TIMEOUT_MS is 250ms by
    // default, but `discover_and_connect` returns Ok even if the
    // connection is still pending; tool discovery may be deferred.
    manager
        .discover_and_connect(servers)
        .await
        .expect("discover_and_connect should not error");

    // Give the server a moment to finish initializing. The manager's
    // default startup timeout is very short (250ms); allow extra time
    // for `npx` to resolve and spawn the process.
    with_timeout(SINGLE_TURN, async {
        loop {
            let tools = manager.get_tools();
            if !tools.is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    })
    .await;

    // ── Assert tools were discovered ───────────────────────────────────
    let tool_source = McpToolSource::new(manager.clone());
    let adapters: Vec<McpToolAdapter> = tool_source
        .into_adapters()
        .await
        .expect("into_adapters should succeed");

    assert!(
        !adapters.is_empty(),
        "expected at least one MCP tool to be discovered from server-everything"
    );

    // Each adapter implements `Tool` and carries a non-empty name.
    for adapter in &adapters {
        let name = Tool::name(adapter);
        assert!(!name.is_empty(), "MCP tool adapter name must not be empty");
    }

    manager
        .disconnect_all()
        .await
        .expect("disconnect_all should succeed");
}
