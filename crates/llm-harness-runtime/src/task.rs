use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use llm_harness_types::EntryId;
use serde_json::Value;

use crate::budget::BudgetControlAdapter;
use crate::cost::CostAggregate;
use crate::error::Result;
use crate::human_approval::HumanApprovalWrapper;
use crate::sandbox::Sandbox;

/// Task identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaskId(pub String);

/// Task-level error.
#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("auth error: {0}")]
    AuthError(String),
    #[error("task cancelled: {0}")]
    Cancelled(String),
    #[error("budget exceeded: limit={limit}, spent={spent}")]
    BudgetExceeded { limit: f64, spent: f64 },
    #[error("retries exhausted: max={max}")]
    RetriesExhausted { max: usize },
    #[error("internal: {0}")]
    Internal(String),
}

/// Task state machine.
#[derive(Debug, Clone)]
pub enum TaskState {
    Pending,
    Running {
        session_id: EntryId,
        started_at: DateTime<Utc>,
    },
    Paused {
        reason: String,
    },
    AwaitingHumanInput {
        request_id: String,
        since: DateTime<Utc>,
    },
    Failed {
        error: String,
        retries: usize,
        ended_at: DateTime<Utc>,
    },
    Succeeded {
        result: TaskResult,
        ended_at: DateTime<Utc>,
    },
    Cancelled {
        reason: String,
    },
}

/// Task completion result.
#[derive(Debug, Clone)]
pub struct TaskResult {
    /// Aggregated cost across all sessions.
    pub cost: CostAggregate,
    /// Total turns executed.
    pub turns: u32,
    /// Final assistant message, if any.
    pub final_message: Option<String>,
}

/// Task-level checkpoint (supplementary to session checkpoints).
#[derive(Debug, Clone)]
pub struct Checkpoint {
    /// Monotonically increasing sequence number.
    pub seq: usize,
    /// When the checkpoint was saved.
    pub saved_at: DateTime<Utc>,
    /// Human-readable description.
    pub description: String,
    /// Arbitrary payload (JSON).
    pub payload: Value,
}

/// Task trace — summary data passed to `TaskVerifier`.
#[derive(Debug, Clone)]
pub struct TaskTrace {
    /// Task identifier.
    pub task_id: TaskId,
    /// All session IDs that belonged to this task.
    pub session_ids: Vec<EntryId>,
    /// All checkpoints recorded during this task.
    pub checkpoints: Vec<Checkpoint>,
    /// Final state snapshot (JSON).
    pub final_state: Value,
}

/// Verdict from a task verifier.
#[derive(Debug, Clone)]
pub enum TaskVerdict {
    Pass,
    Fail { reason: String },
    Indeterminate { reason: String },
}

/// Task completion verifier (business layer implements).
pub trait TaskVerifier: Send + Sync {
    /// Verify task completion and return a verdict.
    fn verify<'a>(&'a self, trace: &'a TaskTrace) -> BoxFuture<'a, TaskVerdict>;
}

/// TaskRunner trait — orchestrates a task lifecycle across one or more
/// `AgentHarness` sessions.
///
/// One task may span multiple harness sessions (e.g., crash recovery with
/// checkpoint resume).
///
/// ## Relationship with AgentHarness
///
/// ```text
/// TaskRunner (lifecycle: entire task)
///   ├─ checkpoints[] (task-level supplementary snapshots)
///   ├─ state (pending → running → succeeded/failed)
///   ├─ retry_policy
///   └─ current_harness: Option<AgentHarness>
/// ```
pub trait TaskRunner: Send + Sync {
    /// Start the task with the given prompt.
    fn start<'a>(&'a self, prompt: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Get current task state.
    fn state(&self) -> TaskState;

    /// Pause the task with a reason.
    fn pause<'a>(&'a self, reason: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Resume a paused task.
    fn resume(&self) -> BoxFuture<'_, Result<()>>;

    /// Cancel the task.
    fn cancel<'a>(&'a self, reason: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Get total cost aggregated across all sessions.
    fn total_cost(&self) -> CostAggregate;

    /// Add a task-level checkpoint.
    fn checkpoint<'a>(&'a self, description: &'a str, payload: Value) -> BoxFuture<'a, Result<()>>;
}

/// Retry policy for `TaskRunnerImpl`.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of retries before marking the task as failed.
    pub max_retries: usize,
    /// Delay between retries in milliseconds.
    pub backoff_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            backoff_ms: 1000,
        }
    }
}

/// Concrete `TaskRunner` managing task lifecycle across one or more sessions.
pub struct TaskRunnerImpl {
    #[allow(dead_code)]
    task_id: TaskId,
    #[allow(dead_code)]
    sandbox: Arc<dyn Sandbox>,
    #[allow(dead_code)]
    verifier: Option<Arc<dyn TaskVerifier>>,
    #[allow(dead_code)]
    retry_policy: RetryPolicy,
    #[allow(dead_code)]
    budget_adapter: Option<Arc<BudgetControlAdapter>>,
    #[allow(dead_code)]
    human_approval: Option<Arc<HumanApprovalWrapper>>,
    state: Mutex<TaskState>,
    cost: Mutex<CostAggregate>,
    checkpoints: Mutex<Vec<Checkpoint>>,
}

impl TaskRunnerImpl {
    /// Create a new runner in `Pending` state.
    pub fn new(
        task_id: TaskId,
        sandbox: Arc<dyn Sandbox>,
        verifier: Option<Arc<dyn TaskVerifier>>,
        retry_policy: RetryPolicy,
        budget_adapter: Option<Arc<BudgetControlAdapter>>,
        human_approval: Option<Arc<HumanApprovalWrapper>>,
    ) -> Self {
        Self {
            task_id,
            sandbox,
            verifier,
            retry_policy,
            budget_adapter,
            human_approval,
            state: Mutex::new(TaskState::Pending),
            cost: Mutex::new(CostAggregate::default()),
            checkpoints: Mutex::new(Vec::new()),
        }
    }
}

impl TaskRunner for TaskRunnerImpl {
    fn start<'a>(&'a self, _prompt: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            *self.state.lock().unwrap() = TaskState::Running {
                session_id: EntryId::new(),
                started_at: Utc::now(),
            };
            Ok(())
        })
    }

    fn state(&self) -> TaskState {
        self.state.lock().unwrap().clone()
    }

    fn pause<'a>(&'a self, reason: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let current = self.state.lock().unwrap().clone();
            match current {
                TaskState::Running { .. } => {
                    *self.state.lock().unwrap() = TaskState::Paused {
                        reason: reason.to_string(),
                    };
                    Ok(())
                }
                _ => Err(anyhow::anyhow!("cannot pause: task is not running")),
            }
        })
    }

    fn resume(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let current = self.state.lock().unwrap().clone();
            match current {
                TaskState::Paused { .. } => {
                    *self.state.lock().unwrap() = TaskState::Running {
                        session_id: EntryId::new(),
                        started_at: Utc::now(),
                    };
                    Ok(())
                }
                _ => Err(anyhow::anyhow!("cannot resume: task is not paused")),
            }
        })
    }

    fn cancel<'a>(&'a self, reason: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            *self.state.lock().unwrap() = TaskState::Cancelled {
                reason: reason.to_string(),
            };
            Ok(())
        })
    }

    fn total_cost(&self) -> CostAggregate {
        self.cost.lock().unwrap().clone()
    }

    fn checkpoint<'a>(&'a self, description: &'a str, payload: Value) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut checkpoints = self.checkpoints.lock().unwrap();
            let seq = checkpoints.len();
            checkpoints.push(Checkpoint {
                seq,
                saved_at: Utc::now(),
                description: description.to_string(),
                payload,
            });
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::BoxFuture;
    use llm_harness_types::{ExecutionEnv, UnsupportedEnv};

    use crate::sandbox::{ResourceLimits, SandboxConfig};

    struct StubSandbox {
        config: SandboxConfig,
    }

    impl Sandbox for StubSandbox {
        fn start(&self) -> BoxFuture<'_, Result<()>> {
            Box::pin(async move { Ok(()) })
        }
        fn env(&self) -> Arc<dyn ExecutionEnv> {
            Arc::new(UnsupportedEnv::new())
        }
        fn reset(&self) -> BoxFuture<'_, Result<()>> {
            Box::pin(async move { Ok(()) })
        }
        fn shutdown(self: Box<Self>) -> BoxFuture<'static, Result<()>> {
            Box::pin(async move { Ok(()) })
        }
        fn config(&self) -> &SandboxConfig {
            &self.config
        }
        fn is_running(&self) -> bool {
            false
        }
    }

    fn make_runner() -> TaskRunnerImpl {
        let config = SandboxConfig {
            fs_allowlist: vec![],
            fs_denylist: vec![],
            net_allowlist: vec![],
            resource_limits: ResourceLimits {
                max_cpus: None,
                max_memory_mb: None,
                max_disk_mb: None,
                timeout: None,
            },
            work_dir: None,
        };
        TaskRunnerImpl::new(
            TaskId("test-task".into()),
            Arc::new(StubSandbox { config }),
            None,
            RetryPolicy::default(),
            None,
            None,
        )
    }

    #[tokio::test]
    async fn task_starts_from_pending() {
        let runner = make_runner();
        assert!(matches!(runner.state(), TaskState::Pending));
        runner.start("do something").await.unwrap();
        assert!(matches!(runner.state(), TaskState::Running { .. }));
    }

    #[tokio::test]
    async fn task_pause_resume_cycle() {
        let runner = make_runner();
        runner.start("do something").await.unwrap();
        runner.pause("waiting for input").await.unwrap();
        assert!(matches!(runner.state(), TaskState::Paused { .. }));
        runner.resume().await.unwrap();
        assert!(matches!(runner.state(), TaskState::Running { .. }));
    }

    #[tokio::test]
    async fn task_cancel_transitions() {
        let runner = make_runner();
        runner.start("do something").await.unwrap();
        runner.cancel("user request").await.unwrap();
        assert!(matches!(runner.state(), TaskState::Cancelled { .. }));
    }

    #[tokio::test]
    async fn cannot_pause_non_running_task() {
        let runner = make_runner();
        let result = runner.pause("test").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not running"));
    }

    #[tokio::test]
    async fn checkpoint_adds_entry() {
        let runner = make_runner();
        runner
            .checkpoint("before edit", serde_json::json!({"file": "main.rs"}))
            .await
            .unwrap();
        runner
            .checkpoint("after edit", serde_json::json!({"file": "main.rs"}))
            .await
            .unwrap();
        let checkpoints = runner.checkpoints.lock().unwrap();
        assert_eq!(checkpoints.len(), 2);
        assert_eq!(checkpoints[0].seq, 0);
        assert_eq!(checkpoints[1].seq, 1);
    }
}
