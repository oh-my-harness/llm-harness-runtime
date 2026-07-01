//! Non-LLM workflow step execution.

use futures::future::BoxFuture;

use crate::error::Result;
use crate::workflow::model::{Step, StepRecord, StepResult};

/// Context passed to a deterministic non-LLM step executor.
pub struct ExecutorCtx<'a> {
    /// Current executor step definition.
    pub current_step: &'a Step,
    /// Previous completed step result, if any.
    pub prev_result: Option<&'a StepResult>,
    /// Completed step history before this executor starts.
    pub step_history: &'a [StepRecord],
}

/// Deterministic executor for a non-LLM workflow step.
pub trait StepExecutor: Send + Sync {
    fn execute<'a>(&'a self, ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>>;
}
