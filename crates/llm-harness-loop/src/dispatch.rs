use std::sync::Arc;

use futures::future::join_all;
use llm_harness_types::*;

/// Partition call indices into sequential sub-groups based on tool execution modes.
///
/// Each `Sequential` tool acts as a barrier: tools before it run in one group,
/// the sequential tool runs alone, and tools after it start a new group.
pub(crate) fn group_by_sequential(
    tools: &[Arc<dyn Tool>],
    default_mode: ToolExecutionMode,
) -> Vec<Vec<usize>> {
    if tools.is_empty() {
        return vec![];
    }
    let mut groups: Vec<Vec<usize>> = vec![vec![]];
    for (i, t) in tools.iter().enumerate() {
        let effective = if t.execution_mode() == ToolExecutionMode::Sequential {
            ToolExecutionMode::Sequential
        } else {
            default_mode
        };
        if effective == ToolExecutionMode::Sequential {
            if !groups.last().unwrap().is_empty() {
                groups.push(vec![]);
            }
            groups.last_mut().unwrap().push(i);
            groups.push(vec![]);
        } else {
            groups.last_mut().unwrap().push(i);
        }
    }
    groups.retain(|g| !g.is_empty());
    groups
}

/// Split `calls` by `Sequential` boundaries and execute groups:
/// parallel within each group, sequential between groups.
///
/// Returns results in the same order as the input `calls` list.
pub(crate) async fn execute_tool_batch(
    calls: Vec<(String, serde_json::Value, Arc<dyn Tool>)>,
    ctx_factory: impl Fn(String) -> ToolContext,
    default_mode: ToolExecutionMode,
) -> Vec<(String, Result<ToolResult, ToolError>)> {
    let tools: Vec<Arc<dyn Tool>> = calls.iter().map(|(_, _, t)| t.clone()).collect();
    let groups = group_by_sequential(&tools, default_mode);

    let n = calls.len();
    let mut results: Vec<Option<(String, Result<ToolResult, ToolError>)>> =
        (0..n).map(|_| None).collect();

    for group in groups {
        let futures: Vec<_> = group
            .iter()
            .map(|&idx| {
                let (tool_use_id, args, tool) = &calls[idx];
                let ctx = ctx_factory(tool_use_id.clone());
                let tool = tool.clone();
                let args = args.clone();
                let id = tool_use_id.clone();
                async move {
                    let r = tool.execute(args, &ctx).await;
                    (idx, id, r)
                }
            })
            .collect();

        for (idx, id, r) in join_all(futures).await {
            results[idx] = Some((id, r));
        }
    }

    results.into_iter().map(|r| r.unwrap()).collect()
}

#[cfg(test)]
#[cfg(feature = "test-utils")]
mod tests {
    use super::*;
    use chrono::Utc;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn parallel_tool(name: &'static str) -> Arc<dyn Tool> {
        struct PT {
            name: &'static str,
        }
        impl Tool for PT {
            fn name(&self) -> &str {
                self.name
            }
            fn description(&self) -> &str {
                ""
            }
            fn parameters_schema(&self) -> &serde_json::Value {
                static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
                S.get_or_init(|| serde_json::json!({}))
            }
            fn execution_mode(&self) -> ToolExecutionMode {
                ToolExecutionMode::Parallel
            }
            fn execute<'a>(
                &'a self,
                _: serde_json::Value,
                _: &'a ToolContext,
            ) -> futures::future::BoxFuture<'a, Result<ToolResult, ToolError>> {
                Box::pin(async {
                    Ok(ToolResult {
                        content: vec![],
                        details: serde_json::Value::Null,
                        terminate: false,
                    })
                })
            }
        }
        Arc::new(PT { name })
    }

    fn sequential_tool(name: &'static str) -> Arc<dyn Tool> {
        struct ST {
            name: &'static str,
        }
        impl Tool for ST {
            fn name(&self) -> &str {
                self.name
            }
            fn description(&self) -> &str {
                ""
            }
            fn parameters_schema(&self) -> &serde_json::Value {
                static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
                S.get_or_init(|| serde_json::json!({}))
            }
            fn execution_mode(&self) -> ToolExecutionMode {
                ToolExecutionMode::Sequential
            }
            fn execute<'a>(
                &'a self,
                _: serde_json::Value,
                _: &'a ToolContext,
            ) -> futures::future::BoxFuture<'a, Result<ToolResult, ToolError>> {
                Box::pin(async {
                    Ok(ToolResult {
                        content: vec![],
                        details: serde_json::Value::Null,
                        terminate: false,
                    })
                })
            }
        }
        Arc::new(ST { name })
    }

    #[test]
    fn group_all_parallel_single_group() {
        let tools: Vec<Arc<dyn Tool>> = vec![parallel_tool("p1"), parallel_tool("p2")];
        let groups = group_by_sequential(&tools, ToolExecutionMode::Parallel);
        assert_eq!(groups, vec![vec![0usize, 1]]);
    }

    #[test]
    fn group_sequential_splits_at_boundary() {
        // [P1, P2, S1, P3] → [[0,1], [2], [3]]
        let tools: Vec<Arc<dyn Tool>> = vec![
            parallel_tool("p1"),
            parallel_tool("p2"),
            sequential_tool("s1"),
            parallel_tool("p3"),
        ];
        let groups = group_by_sequential(&tools, ToolExecutionMode::Parallel);
        assert_eq!(groups, vec![vec![0, 1], vec![2], vec![3]]);
    }

    #[test]
    fn group_all_sequential_each_own_group() {
        let tools: Vec<Arc<dyn Tool>> = vec![sequential_tool("s1"), sequential_tool("s2")];
        let groups = group_by_sequential(&tools, ToolExecutionMode::Sequential);
        assert_eq!(groups, vec![vec![0], vec![1]]);
    }

    #[test]
    fn group_empty_returns_empty() {
        let groups = group_by_sequential(&[], ToolExecutionMode::Parallel);
        assert!(groups.is_empty());
    }

    #[tokio::test]
    async fn execute_batch_all_succeed() {
        let t1 = parallel_tool("t1");
        let t2 = parallel_tool("t2");
        let env: Arc<dyn ExecutionEnv> = Arc::new(crate::test_utils::NoOpEnv);
        let assistant_message = Arc::new(AssistantMessage {
            content: vec![],
            stop_reason: None,
            timestamp: Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        });
        let calls = vec![
            ("c1".to_string(), serde_json::json!({}), t1),
            ("c2".to_string(), serde_json::json!({}), t2),
        ];
        let env_clone = env.clone();
        let msg_clone = assistant_message.clone();
        let results = execute_tool_batch(
            calls,
            move |tool_use_id| {
                let (tx, _) = mpsc::channel(1);
                ToolContext {
                    env: env_clone.clone(),
                    abort: CancellationToken::new(),
                    tool_use_id,
                    turn_index: 0,
                    assistant_message: msg_clone.clone(),
                    update_tx: tx,
                }
            },
            ToolExecutionMode::Parallel,
        )
        .await;
        assert_eq!(results.len(), 2);
        assert!(results[0].1.is_ok());
        assert!(results[1].1.is_ok());
    }
}
