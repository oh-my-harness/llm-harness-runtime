//! Non-LLM workflow step execution.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::{Map, Value};

use crate::cost::CostAggregate;
use crate::error::Result;
use crate::workflow::model::{Step, StepRecord, StepResult};

pub const JSON_TRANSFORM_EXECUTOR_NAME: &str = "json_transform";

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

/// Built-in deterministic executors shipped with the runtime.
pub fn builtin_executors() -> impl Iterator<Item = (&'static str, Arc<dyn StepExecutor>)> {
    [(
        JSON_TRANSFORM_EXECUTOR_NAME,
        Arc::new(JsonTransformExecutor) as Arc<dyn StepExecutor>,
    )]
    .into_iter()
}

/// JSON transform executor.
///
/// Config schema:
/// - `source`: optional, one of `auto`, `config.input`, `prev_result.structured`,
///   `prev_result.output`. Default is `auto`.
/// - `input`: JSON value used when `source = "config.input"` or by `auto`.
/// - `pointer`: optional RFC 6901 JSON Pointer applied to the selected input.
/// - `fields`: optional object mapping output field names to either a pointer string,
///   `{ "pointer": "...", "default": ... }`, or a literal value.
/// - `include_nulls`: optional bool, defaults to true. When false, null fields are omitted.
#[derive(Debug, Default)]
pub struct JsonTransformExecutor;

impl StepExecutor for JsonTransformExecutor {
    fn execute<'a>(&'a self, ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
        Box::pin(async move {
            let config = ctx
                .current_step
                .executor_config()
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            let config_obj = config
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("json_transform config must be an object"))?;
            let input = select_input(config_obj, ctx)?;
            let transformed = if let Some(fields) = config_obj.get("fields") {
                build_fields(fields, &input, config_obj)?
            } else if let Some(pointer) = config_obj.get("pointer") {
                let pointer = pointer
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("json_transform.pointer must be a string"))?;
                read_pointer(&input, pointer)?.clone()
            } else {
                input
            };
            let output = serde_json::to_string(&transformed)?;
            Ok(StepResult {
                output,
                structured: Some(transformed),
                tool_calls_count: 0,
                session_id: String::new(),
                cost: CostAggregate::default(),
                started_at: None,
                ended_at: None,
            })
        })
    }
}

fn select_input(config: &Map<String, Value>, ctx: &ExecutorCtx<'_>) -> Result<Value> {
    let source = config
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("auto");
    match source {
        "auto" => {
            if let Some(input) = config.get("input") {
                Ok(input.clone())
            } else if let Some(prev) = ctx.prev_result {
                Ok(prev
                    .structured
                    .clone()
                    .unwrap_or_else(|| Value::String(prev.output.clone())))
            } else {
                anyhow::bail!("json_transform requires config.input or a previous result")
            }
        }
        "config.input" => config
            .get("input")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("json_transform source config.input requires input")),
        "prev_result.structured" => ctx
            .prev_result
            .and_then(|prev| prev.structured.clone())
            .ok_or_else(|| anyhow::anyhow!("json_transform requires prev_result.structured")),
        "prev_result.output" => ctx
            .prev_result
            .map(|prev| Value::String(prev.output.clone()))
            .ok_or_else(|| anyhow::anyhow!("json_transform requires prev_result.output")),
        other => anyhow::bail!("json_transform source '{}' is not supported", other),
    }
}

fn build_fields(fields: &Value, input: &Value, config: &Map<String, Value>) -> Result<Value> {
    let fields = fields
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("json_transform.fields must be an object"))?;
    let include_nulls = config
        .get("include_nulls")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let mut output = Map::new();
    for (name, spec) in fields {
        let value = resolve_field_spec(spec, input)?;
        if include_nulls || !value.is_null() {
            output.insert(name.clone(), value);
        }
    }
    Ok(Value::Object(output))
}

fn resolve_field_spec(spec: &Value, input: &Value) -> Result<Value> {
    if let Some(pointer) = spec.as_str() {
        return Ok(read_pointer(input, pointer)?.clone());
    }
    if let Some(spec_obj) = spec.as_object() {
        if let Some(pointer) = spec_obj.get("pointer") {
            let pointer = pointer
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("json_transform field pointer must be a string"))?;
            return match read_pointer(input, pointer) {
                Ok(value) => Ok(value.clone()),
                Err(err) => {
                    if let Some(default) = spec_obj.get("default") {
                        Ok(default.clone())
                    } else {
                        Err(err)
                    }
                }
            };
        }
        if let Some(value) = spec_obj.get("value") {
            return Ok(value.clone());
        }
    }
    Ok(spec.clone())
}

fn read_pointer<'a>(input: &'a Value, pointer: &str) -> Result<&'a Value> {
    input
        .pointer(pointer)
        .ok_or_else(|| anyhow::anyhow!("json_transform pointer '{}' not found", pointer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executor_step(config: Value) -> Step {
        Step::executor(
            "transform",
            "Transform",
            JSON_TRANSFORM_EXECUTOR_NAME,
            Some(config),
        )
    }

    fn prev_result(structured: Option<Value>, output: impl Into<String>) -> StepResult {
        StepResult {
            output: output.into(),
            structured,
            tool_calls_count: 0,
            session_id: "prev".into(),
            cost: CostAggregate::default(),
            started_at: None,
            ended_at: None,
        }
    }

    #[tokio::test]
    async fn json_transform_selects_pointer_from_config_input() {
        let step = executor_step(serde_json::json!({
            "source": "config.input",
            "input": {"device": {"voltage": 3.3}},
            "pointer": "/device/voltage"
        }));
        let ctx = ExecutorCtx {
            current_step: &step,
            prev_result: None,
            step_history: &[],
        };

        let result = JsonTransformExecutor.execute(&ctx).await.unwrap();

        assert_eq!(result.structured, Some(serde_json::json!(3.3)));
        assert_eq!(result.output, "3.3");
    }

    #[tokio::test]
    async fn json_transform_builds_fields_from_previous_structured_result() {
        let prev = prev_result(
            Some(serde_json::json!({
                "user": {"id": 7, "name": "Ada"},
                "status": "ok"
            })),
            "ignored",
        );
        let step = executor_step(serde_json::json!({
            "fields": {
                "id": "/user/id",
                "name": {"pointer": "/user/name"},
                "missing": {"pointer": "/not/found", "default": "fallback"},
                "constant": {"value": true}
            }
        }));
        let ctx = ExecutorCtx {
            current_step: &step,
            prev_result: Some(&prev),
            step_history: &[],
        };

        let result = JsonTransformExecutor.execute(&ctx).await.unwrap();

        assert_eq!(
            result.structured,
            Some(serde_json::json!({
                "id": 7,
                "name": "Ada",
                "missing": "fallback",
                "constant": true
            }))
        );
    }

    #[tokio::test]
    async fn json_transform_rejects_missing_pointer_without_default() {
        let step = executor_step(serde_json::json!({
            "source": "config.input",
            "input": {"value": 1},
            "fields": {"bad": "/missing"}
        }));
        let ctx = ExecutorCtx {
            current_step: &step,
            prev_result: None,
            step_history: &[],
        };

        let err = JsonTransformExecutor.execute(&ctx).await.unwrap_err();

        assert!(err.to_string().contains("pointer '/missing' not found"));
    }
}
