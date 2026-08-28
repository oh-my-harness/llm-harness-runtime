//! Hooks for the agent team:
//!
//! - [`TeamBeforeRunHook`]: injects a todolist snapshot at the start of each
//!   `harness.run()` call, so the LLM sees its current tasks in the first
//!   turn's context.
use futures::future::BoxFuture;
use llm_harness_types::{
    AgentError, AgentMessage, BeforeRunCtx, BeforeRunHook, BeforeRunResult, ContentBlock,
    UserMessage,
};

use llm_harness_strategy::status_panel::TodoStore;

/// Injects a todolist snapshot as an additional user message at the start of
/// each `harness.run()` call.
///
/// The snapshot is formatted by [`TodoStore::format_snapshot`] and appended to
/// `additional_messages`. The LLM sees it in its first turn's context. It is
/// not re-injected on subsequent turns within the same run — the LLM can
/// check its todolist via the `todo_write` tool result.
pub struct TeamBeforeRunHook {
    todo_store: TodoStore,
}

impl TeamBeforeRunHook {
    /// Create a hook backed by the given todo store.
    pub fn new(todo_store: TodoStore) -> Self {
        Self { todo_store }
    }
}

impl BeforeRunHook for TeamBeforeRunHook {
    fn before_run<'a>(
        &'a self,
        _ctx: BeforeRunCtx<'a>,
    ) -> BoxFuture<'a, Result<BeforeRunResult, AgentError>> {
        Box::pin(async move {
            let snapshot = self.todo_store.format_snapshot();
            let message = AgentMessage::User(UserMessage {
                content: vec![ContentBlock::Text { text: snapshot }],
                timestamp: chrono::Utc::now(),
            });
            Ok(BeforeRunResult {
                additional_messages: vec![message],
                system_prompt: None,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_strategy::status_panel::{TodoItem, TodoStatus};
    use llm_harness_types::{AgentHarnessResources, RunContext, RunRequest};

    fn make_ctx() -> (RunContext, AgentHarnessResources) {
        (
            RunContext::new(RunRequest::from_text("do the task")),
            AgentHarnessResources {
                skills: vec![],
                templates: vec![],
            },
        )
    }

    #[tokio::test]
    async fn before_run_injects_todolist_snapshot() {
        let store = TodoStore::new();
        store.add(TodoItem {
            id: "1".into(),
            content: "Write tests".into(),
            status: TodoStatus::Pending,
            updated_at: chrono::Utc::now(),
        });

        let hook = TeamBeforeRunHook::new(store);

        let (run, resources) = make_ctx();
        let result = hook
            .before_run(BeforeRunCtx {
                run: &run,
                prompt_text: "do the task",
                initial_messages: &mut Vec::new(),
                system_prompt: &mut None,
                resources: &resources,
            })
            .await
            .expect("hook must not fail");

        // Exactly one additional user message carrying the snapshot.
        assert_eq!(result.additional_messages.len(), 1);
        assert!(matches!(
            &result.additional_messages[0],
            AgentMessage::User(u) if u.content.iter().any(
                |c| matches!(c, ContentBlock::Text { text } if text.contains("Write tests"))
            )
        ));
        // Snapshot must NOT touch the system prompt.
        assert_eq!(result.system_prompt, None);
    }

    #[tokio::test]
    async fn before_run_empty_todolist_still_injects() {
        let store = TodoStore::new();
        let hook = TeamBeforeRunHook::new(store);

        let (run, resources) = make_ctx();
        let result = hook
            .before_run(BeforeRunCtx {
                run: &run,
                prompt_text: "do the task",
                initial_messages: &mut Vec::new(),
                system_prompt: &mut None,
                resources: &resources,
            })
            .await
            .expect("hook must not fail");

        assert_eq!(result.additional_messages.len(), 1);
        assert!(
            result.additional_messages.iter().any(|m| matches!(
                m,
                AgentMessage::User(u) if u.content.iter().any(
                    |c| matches!(c, ContentBlock::Text { text } if text.contains("empty"))
                )
            )),
            "empty todolist must still inject its snapshot marker"
        );
    }
}
