use std::path::Path;

pub mod bash;
pub mod edit;
pub mod grep;
pub mod read;
pub mod write;

/// Maximum combined stdout+stderr bytes returned by a single tool call.
pub const MAX_OUTPUT_BYTES: usize = 100 * 1024;

/// Phase 1 tool names in declaration order.
pub const PHASE_1_TOOLS: &[&str] = &["read", "bash", "edit", "write", "grep"];

/// Resolve a file path: absolute paths are returned as-is; relative paths are
/// joined with the given working directory.
pub fn resolve_path(path: &str, cwd: &Path) -> std::path::PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use std::sync::Arc;

    use llm_harness_types::{AssistantMessage, ExecutionEnv, ToolContext};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// Create a `ToolContext` for testing with the given environment.
    pub(crate) fn make_ctx(env: Arc<dyn ExecutionEnv>) -> ToolContext {
        let (tx, _rx) = mpsc::channel(4);
        ToolContext {
            env,
            abort: CancellationToken::new(),
            tool_use_id: "test-id".into(),
            turn_index: 0,
            assistant_message: Arc::new(AssistantMessage {
                kind: llm_harness_types::AssistantMessageKind::FinalAnswer,
                content: vec![],
                stop_reason: None,
                timestamp: chrono::Utc::now(),
                provider: None,
                api: None,
                model: None,
                usage: None,
                error_message: None,
            }),
            update_tx: tx,
        }
    }

    /// Create a `ToolContext` for testing with a receiver channel, needed when
    /// the test inspects `ToolResult` updates sent via the channel.
    pub(crate) fn make_ctx_with_rx(
        env: Arc<dyn ExecutionEnv>,
    ) -> (ToolContext, mpsc::Receiver<llm_harness_types::ToolResult>) {
        let (tx, rx) = mpsc::channel(4);
        let ctx = ToolContext {
            env,
            abort: CancellationToken::new(),
            tool_use_id: "test-id".into(),
            turn_index: 0,
            assistant_message: Arc::new(AssistantMessage {
                kind: llm_harness_types::AssistantMessageKind::FinalAnswer,
                content: vec![],
                stop_reason: None,
                timestamp: chrono::Utc::now(),
                provider: None,
                api: None,
                model: None,
                usage: None,
                error_message: None,
            }),
            update_tx: tx,
        };
        (ctx, rx)
    }
}

mod tests {
    #[test]
    fn phase_1_has_five_tools() {
        assert_eq!(super::PHASE_1_TOOLS.len(), 5);
    }

    #[test]
    fn phase_1_tools_are_unique() {
        let mut sorted: Vec<&str> = super::PHASE_1_TOOLS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), super::PHASE_1_TOOLS.len());
    }

    #[test]
    fn phase_1_includes_grep() {
        assert!(
            super::PHASE_1_TOOLS.contains(&"grep"),
            "PHASE_1_TOOLS should include grep"
        );
    }
}
