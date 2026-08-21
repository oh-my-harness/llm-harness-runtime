// crates/llm-harness-workflow/src/task_store.rs
//! Task 持久化存储抽象。单进程独占（无需跨进程锁）。
//!
//! 参照 `SessionStorage`/`SessionRepo`/`JsonlSessionStorage` 三件套，
//! task 层补同等的持久化抽象。一个 task 的持久化状态分三类 + 事件等待，
//! 共享同一个 `TaskStore`。

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::lifecycle::task::{Checkpoint, TaskId};
use crate::workflow::model::{StepId, Workflow, WorkflowState, WorkflowStatus};
use anyhow::Result;

/// 事件等待记录（descriptor + runtime 写入的时间戳）。
/// 业务层用 saved_at 判断 descriptor 是否过期（如崩溃后很久才恢复）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventWaitRecord {
    /// 业务层写入的等待描述符（runtime 不懂语义）。
    pub descriptor: Value,
    /// runtime 在 save_event_wait 时写入的 UTC 时间戳。
    pub saved_at: chrono::DateTime<chrono::Utc>,
}

/// Task 摘要（`list_tasks()` 返回）。
///
/// 包含从 `<task_id>/workflow.json` 读取的进度摘要信息，不含完整
/// step_history 或 checkpoint payload。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    /// Task ID（形如 `task-<uuid>`）。
    pub task_id: TaskId,
    /// 生命周期状态（Idle/Running/Paused/Succeeded/Failed/Cancelled）。
    pub status: WorkflowStatus,
    /// 当前步骤。
    pub current_step: StepId,
    /// 已完成步骤数（step_history 长度）。
    pub step_count: usize,
    /// 启动时间（status != Idle 时有值）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// 结束时间（status ∈ {Succeeded, Failed, Cancelled} 时有值）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    /// 暂停/失败/取消原因。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// 规划此 workflow 的 task ID（两阶段 LLM 场景）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_by: Option<TaskId>,
}

/// Task 持久化存储抽象。单进程独占（无需跨进程锁）。
/// payload 用 serde_json::Value，与现有 Checkpoint.payload 同构。
///
/// 职责边界：
/// - `WorkflowState`（status/current_step/step_history）是引擎管的"当前进度"，覆盖写。
/// - `Checkpoint` 是业务层管的"业务数据快照"，append-only。两者不重叠。
/// - `event_wait` 的 descriptor 是 `serde_json::Value`——runtime 不懂语义。
pub trait TaskStore: Send + Sync {
    /// workflow 状态（生命周期 status + current_step + 步骤历史 + result/error）。
    /// None 表示该 task 未持久化（新 task，正常状态）。
    fn load_workflow_state(&self, task_id: &TaskId)
    -> BoxFuture<'_, Result<Option<WorkflowState>>>;
    fn save_workflow_state(
        &self,
        task_id: &TaskId,
        state: &WorkflowState,
    ) -> BoxFuture<'_, Result<()>>;

    /// workflow 定义本身（steps + edges + entry_step），即 plan。
    /// 来源无关：PlanWorkflowTool（LLM 产出）/ new()（人类构造）/ 业务层主动写入，都走此接口落盘。
    /// 崩溃恢复时 restore() 据此重建 Workflow，再叠加 WorkflowState 续跑。
    fn load_workflow(&self, task_id: &TaskId) -> BoxFuture<'_, Result<Option<Workflow>>>;
    fn save_workflow(&self, task_id: &TaskId, workflow: &Workflow) -> BoxFuture<'_, Result<()>>;

    /// task checkpoints（append-only，与现有 Checkpoint 同构）。
    /// ⚠ Checkpoint 的 payload 存的是引擎不理解的业务数据。任务进度
    /// 由 WorkflowState 单一表达，Checkpoint 不复制进度信息。
    fn list_checkpoints(&self, task_id: &TaskId) -> BoxFuture<'_, Result<Vec<Checkpoint>>>;
    fn save_checkpoint(
        &self,
        task_id: &TaskId,
        checkpoint: &Checkpoint,
    ) -> BoxFuture<'_, Result<()>>;

    /// 事件等待状态（tool 等待描述符，业务层写入）。
    /// None 表示该 task 未在等待事件。
    /// runtime 不懂描述符语义，只负责存取 Value + 时间戳。
    fn load_event_wait(&self, task_id: &TaskId) -> BoxFuture<'_, Result<Option<EventWaitRecord>>>;
    fn save_event_wait(&self, task_id: &TaskId, descriptor: &Value) -> BoxFuture<'_, Result<()>>;
    fn clear_event_wait(&self, task_id: &TaskId) -> BoxFuture<'_, Result<()>>;

    /// 清理（可选，task 完成后调用）。
    fn delete(&self, task_id: &TaskId) -> BoxFuture<'_, Result<()>>;

    /// 枚举 store 中所有 task，返回摘要列表。
    ///
    /// 扫描 `base_dir` 的子目录，读取每个 `<task_id>/workflow.json` 获取
    /// 状态摘要。无法读取的目录（损坏的 json、非 task 目录）会被跳过。
    fn list_tasks(&self) -> BoxFuture<'_, Result<Vec<TaskSummary>>>;
}

/// 基于 JSONL 文件的 `TaskStore` 参考实现。
///
/// 与 `JsonlSessionStorage` 同构：单进程独占（`tokio::sync::Mutex` 保护），
/// 崩溃恢复是"旧进程确认死后，新进程 load"。
///
/// 落盘结构：
/// ```text
/// <base_dir>/<task_id>/
/// ├─ plan.json           # Workflow 定义，覆盖写
/// ├─ workflow.json       # WorkflowState，覆盖写（原子：写临时文件 + rename）
/// ├─ checkpoints.jsonl   # append-only
/// └─ event_wait.json     # EventWaitRecord，有等待时存在
/// ```
pub struct JsonlTaskStore {
    base_dir: PathBuf,
    /// 每 task 一个 mutex，串行化该 task 目录的写操作。
    locks: Mutex<std::collections::HashMap<String, Arc<Mutex<()>>>>,
}

impl JsonlTaskStore {
    /// 创建 store。`base_dir` 是所有 task 数据的根目录。
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            locks: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 获取（或创建）指定 task 的目录。
    /// 校验 task_id 不含路径分隔符或 `..`，防止路径遍历攻击。
    fn task_dir(&self, task_id: &TaskId) -> Result<PathBuf> {
        Self::validate_task_id(task_id)?;
        Ok(self.base_dir.join(&task_id.0))
    }

    /// 校验 task_id.0 不含路径分隔符、`..` 或空字节。
    fn validate_task_id(task_id: &TaskId) -> Result<()> {
        if task_id.0.is_empty() {
            anyhow::bail!("invalid task_id: empty");
        }
        if task_id.0.contains('/') || task_id.0.contains('\\') {
            anyhow::bail!("invalid task_id: contains path separator");
        }
        if task_id.0.contains("..") {
            anyhow::bail!("invalid task_id: contains path traversal");
        }
        if task_id.0.contains('\0') {
            anyhow::bail!("invalid task_id: contains null byte");
        }
        Ok(())
    }

    /// 获取该 task 专属的 mutex（串行化写）。
    /// 接受 owned String 以避免借用生命周期纠缠 async 块。
    async fn task_lock(&self, key: String) -> Arc<Mutex<()>> {
        let mut guards = self.locks.lock().await;
        guards
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// 原子写：写临时文件 + rename（避免崩溃时半写文件）。
    async fn atomic_write(path: &std::path::Path, content: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, content).await?;
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }
}

impl TaskStore for JsonlTaskStore {
    fn load_workflow_state(
        &self,
        task_id: &TaskId,
    ) -> BoxFuture<'_, Result<Option<WorkflowState>>> {
        let task_id = task_id.clone();
        Box::pin(async move {
            let path = self.task_dir(&task_id)?.join("workflow.json");
            match tokio::fs::read(&path).await {
                Ok(bytes) => {
                    let state: WorkflowState = serde_json::from_slice(&bytes)?;
                    Ok(Some(state))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
    }

    fn save_workflow_state(
        &self,
        task_id: &TaskId,
        state: &WorkflowState,
    ) -> BoxFuture<'_, Result<()>> {
        let task_id = task_id.clone();
        let state = state.clone();
        Box::pin(async move {
            let path = self.task_dir(&task_id)?.join("workflow.json");
            let key = task_id.0.clone();
            let content = serde_json::to_string(&state)?;
            let lock = self.task_lock(key).await;
            let _g = lock.lock().await;
            Self::atomic_write(&path, &content).await
        })
    }

    fn load_workflow(&self, task_id: &TaskId) -> BoxFuture<'_, Result<Option<Workflow>>> {
        let task_id = task_id.clone();
        Box::pin(async move {
            let path = self.task_dir(&task_id)?.join("plan.json");
            match tokio::fs::read(&path).await {
                Ok(bytes) => {
                    let wf: Workflow = serde_json::from_slice(&bytes)?;
                    Ok(Some(wf))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
    }
    fn save_workflow(&self, task_id: &TaskId, workflow: &Workflow) -> BoxFuture<'_, Result<()>> {
        let task_id = task_id.clone();
        let workflow = workflow.clone();
        Box::pin(async move {
            let path = self.task_dir(&task_id)?.join("plan.json");
            let key = task_id.0.clone();
            let content = serde_json::to_string(&workflow)?;
            let lock = self.task_lock(key).await;
            let _g = lock.lock().await;
            Self::atomic_write(&path, &content).await
        })
    }

    fn list_checkpoints(&self, task_id: &TaskId) -> BoxFuture<'_, Result<Vec<Checkpoint>>> {
        let task_id = task_id.clone();
        Box::pin(async move {
            let path = self.task_dir(&task_id)?.join("checkpoints.jsonl");
            match tokio::fs::read_to_string(&path).await {
                Ok(content) => {
                    let mut cps = Vec::new();
                    for line in content.lines() {
                        if line.is_empty() {
                            continue;
                        }
                        let cp: Checkpoint = serde_json::from_str(line)?;
                        cps.push(cp);
                    }
                    Ok(cps)
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
                Err(e) => Err(e.into()),
            }
        })
    }
    fn save_checkpoint(
        &self,
        task_id: &TaskId,
        checkpoint: &Checkpoint,
    ) -> BoxFuture<'_, Result<()>> {
        let task_id = task_id.clone();
        let checkpoint = checkpoint.clone();
        Box::pin(async move {
            let path = self.task_dir(&task_id)?.join("checkpoints.jsonl");
            let key = task_id.0.clone();
            let line = serde_json::to_string(&checkpoint)?;
            let lock = self.task_lock(key).await;
            let _g = lock.lock().await;
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            // append-only：追加一行 + 换行
            let mut content = line;
            content.push('\n');
            use tokio::io::AsyncWriteExt;
            let mut f = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await?;
            f.write_all(content.as_bytes()).await?;
            f.flush().await?;
            Ok(())
        })
    }

    fn load_event_wait(&self, task_id: &TaskId) -> BoxFuture<'_, Result<Option<EventWaitRecord>>> {
        let task_id = task_id.clone();
        Box::pin(async move {
            let path = self.task_dir(&task_id)?.join("event_wait.json");
            match tokio::fs::read(&path).await {
                Ok(bytes) => {
                    let rec: EventWaitRecord = serde_json::from_slice(&bytes)?;
                    Ok(Some(rec))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
    }
    fn save_event_wait(&self, task_id: &TaskId, descriptor: &Value) -> BoxFuture<'_, Result<()>> {
        let task_id = task_id.clone();
        let descriptor = descriptor.clone();
        Box::pin(async move {
            let path = self.task_dir(&task_id)?.join("event_wait.json");
            let key = task_id.0.clone();
            let rec = EventWaitRecord {
                descriptor,
                saved_at: Utc::now(),
            };
            let content = serde_json::to_string(&rec)?;
            let lock = self.task_lock(key).await;
            let _g = lock.lock().await;
            Self::atomic_write(&path, &content).await
        })
    }

    fn clear_event_wait(&self, task_id: &TaskId) -> BoxFuture<'_, Result<()>> {
        let task_id = task_id.clone();
        Box::pin(async move {
            let path = self.task_dir(&task_id)?.join("event_wait.json");
            let key = task_id.0.clone();
            let lock = self.task_lock(key).await;
            let _g = lock.lock().await;
            match tokio::fs::remove_file(&path).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            }
        })
    }
    fn delete(&self, task_id: &TaskId) -> BoxFuture<'_, Result<()>> {
        let task_id = task_id.clone();
        Box::pin(async move {
            let dir = self.task_dir(&task_id)?;
            let key = task_id.0.clone();
            let lock = self.task_lock(key).await;
            let _g = lock.lock().await;
            let result = match tokio::fs::remove_dir_all(&dir).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            };
            // 清理 HashMap 中的 lock entry，避免长期运行后无用的 Arc<Mutex> 累积。
            // 安全：in-flight 操作持有自己的 Arc clone，remove 只从 HashMap 移除 entry，
            // 不影响已存在的锁。新操作会通过 or_insert_with 重建 entry。
            self.locks.lock().await.remove(&task_id.0);
            result
        })
    }

    fn list_tasks(&self) -> BoxFuture<'_, Result<Vec<TaskSummary>>> {
        Box::pin(async move {
            let mut summaries = Vec::new();
            // Scan base_dir for subdirectories — each is a potential task.
            let mut entries = match tokio::fs::read_dir(&self.base_dir).await {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // base_dir doesn't exist yet — no tasks.
                    return Ok(summaries);
                }
                Err(e) => return Err(e.into()),
            };

            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                // Extract task_id from directory name.
                let task_id_str = match path.file_name().and_then(|n| n.to_str()) {
                    Some(name) => name.to_string(),
                    None => continue,
                };
                let task_id = TaskId(task_id_str);
                if Self::validate_task_id(&task_id).is_err() {
                    continue;
                }

                // Read workflow.json for the state summary.
                let workflow_path = path.join("workflow.json");
                let json = match tokio::fs::read_to_string(&workflow_path).await {
                    Ok(json) => json,
                    Err(_) => continue,
                };
                let state: WorkflowState = match serde_json::from_str(&json) {
                    Ok(state) => state,
                    Err(_) => continue,
                };

                summaries.push(TaskSummary {
                    task_id,
                    status: state.status,
                    current_step: state.current_step,
                    step_count: state.step_history.len(),
                    started_at: state.started_at,
                    ended_at: state.ended_at,
                    reason: state.reason,
                    planned_by: state.planned_by,
                });
            }

            // Sort by started_at descending (most recent first).
            summaries.sort_by_key(|t| std::cmp::Reverse(t.started_at));

            Ok(summaries)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::{Step, Workflow, WorkflowState, WorkflowStatus};
    use tempfile::TempDir;

    #[test]
    fn event_wait_record_roundtrip() {
        let r = EventWaitRecord {
            descriptor: serde_json::json!({"source": "ci", "job_id": "#12345"}),
            saved_at: Utc::now(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: EventWaitRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.descriptor["job_id"], "#12345");
    }

    fn make_store() -> (TempDir, Arc<JsonlTaskStore>) {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
        (dir, store)
    }

    fn make_workflow() -> Workflow {
        Workflow {
            entry_step: "s1".into(),
            steps: vec![Step::llm("s1", "Step 1", "do step 1", vec![])],
            edges: vec![],
        }
    }

    #[tokio::test]
    async fn load_missing_returns_none() {
        let (_dir, store) = make_store();
        let id = TaskId("nope".into());
        assert!(store.load_workflow_state(&id).await.unwrap().is_none());
        assert!(store.load_workflow(&id).await.unwrap().is_none());
        assert!(store.load_event_wait(&id).await.unwrap().is_none());
        assert!(store.list_checkpoints(&id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn save_and_load_workflow_state() {
        let (_dir, store) = make_store();
        let id = TaskId("t1".into());
        let state = WorkflowState::new("s1".into());
        store.save_workflow_state(&id, &state).await.unwrap();
        let loaded = store.load_workflow_state(&id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WorkflowStatus::Idle);
        assert_eq!(loaded.current_step, "s1");
    }

    #[tokio::test]
    async fn save_and_load_workflow() {
        let (_dir, store) = make_store();
        let id = TaskId("t1".into());
        let wf = make_workflow();
        store.save_workflow(&id, &wf).await.unwrap();
        let loaded = store.load_workflow(&id).await.unwrap().unwrap();
        assert_eq!(loaded.entry_step, "s1");
        assert_eq!(loaded.steps.len(), 1);
    }

    #[tokio::test]
    async fn save_and_load_event_wait() {
        let (_dir, store) = make_store();
        let id = TaskId("t1".into());
        let desc = serde_json::json!({"source": "ci"});
        store.save_event_wait(&id, &desc).await.unwrap();
        let rec = store.load_event_wait(&id).await.unwrap().unwrap();
        assert_eq!(rec.descriptor["source"], "ci");
        // saved_at 由 runtime 写入
        assert!(rec.saved_at <= Utc::now());

        store.clear_event_wait(&id).await.unwrap();
        assert!(store.load_event_wait(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn checkpoints_append_only() {
        let (_dir, store) = make_store();
        let id = TaskId("t1".into());
        let cp1 = Checkpoint {
            seq: 0,
            saved_at: Utc::now(),
            description: "first".into(),
            payload: serde_json::json!({"v": 1}),
        };
        let cp2 = Checkpoint {
            seq: 1,
            saved_at: Utc::now(),
            description: "second".into(),
            payload: serde_json::json!({"v": 2}),
        };
        store.save_checkpoint(&id, &cp1).await.unwrap();
        store.save_checkpoint(&id, &cp2).await.unwrap();
        let list = store.list_checkpoints(&id).await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].description, "first");
        assert_eq!(list[1].payload["v"], 2);
    }

    #[tokio::test]
    async fn overwrite_workflow_state_is_latest() {
        let (_dir, store) = make_store();
        let id = TaskId("t1".into());
        let mut state = WorkflowState::new("s1".into());
        store.save_workflow_state(&id, &state).await.unwrap();
        // 覆盖写：推进到 Running
        state.status = WorkflowStatus::Running;
        state.current_step = "s2".into();
        store.save_workflow_state(&id, &state).await.unwrap();
        let loaded = store.load_workflow_state(&id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WorkflowStatus::Running);
        assert_eq!(loaded.current_step, "s2");
    }

    #[tokio::test]
    async fn delete_removes_all() {
        let (_dir, store) = make_store();
        let id = TaskId("t1".into());
        store.save_workflow(&id, &make_workflow()).await.unwrap();
        store
            .save_workflow_state(&id, &WorkflowState::new("s1".into()))
            .await
            .unwrap();
        store.delete(&id).await.unwrap();
        assert!(store.load_workflow(&id).await.unwrap().is_none());
        assert!(store.load_workflow_state(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_tasks_empty_when_no_dir() {
        let (_dir, store) = make_store();
        let tasks = store.list_tasks().await.unwrap();
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn list_tasks_returns_all_tasks() {
        let (_dir, store) = make_store();
        let id1 = TaskId("task-aaa".into());
        let id2 = TaskId("task-bbb".into());

        let mut state1 = WorkflowState::new("s1".into());
        state1.status = WorkflowStatus::Succeeded;
        state1.started_at = Some(Utc::now());
        store.save_workflow_state(&id1, &state1).await.unwrap();

        let state2 = WorkflowState::new("s1".into());
        store.save_workflow_state(&id2, &state2).await.unwrap();

        let tasks = store.list_tasks().await.unwrap();
        assert_eq!(tasks.len(), 2);

        // All tasks have correct ids
        let ids: Vec<String> = tasks.iter().map(|t| t.task_id.0.clone()).collect();
        assert!(ids.contains(&"task-aaa".to_string()));
        assert!(ids.contains(&"task-bbb".to_string()));

        // task-aaa should have Succeeded status
        let task1 = tasks.iter().find(|t| t.task_id.0 == "task-aaa").unwrap();
        assert_eq!(task1.status, WorkflowStatus::Succeeded);
        assert!(task1.started_at.is_some());
    }

    #[tokio::test]
    async fn list_tasks_skips_corrupt_dirs() {
        let (dir, store) = make_store();
        let id = TaskId("task-good".into());
        store
            .save_workflow_state(&id, &WorkflowState::new("s1".into()))
            .await
            .unwrap();

        // Create a corrupt task dir with invalid workflow.json
        let bad_dir = dir.path().join("task-bad");
        tokio::fs::create_dir_all(&bad_dir).await.unwrap();
        tokio::fs::write(bad_dir.join("workflow.json"), "not json")
            .await
            .unwrap();

        // Create a dir without workflow.json (not a task)
        let empty_dir = dir.path().join("task-empty");
        tokio::fs::create_dir_all(&empty_dir).await.unwrap();

        let tasks = store.list_tasks().await.unwrap();
        // Only the good task should be listed
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id.0, "task-good");
    }

    #[tokio::test]
    async fn list_tasks_sorted_by_started_at_desc() {
        let (_dir, store) = make_store();
        let id1 = TaskId("task-old".into());
        let id2 = TaskId("task-new".into());

        let mut state1 = WorkflowState::new("s1".into());
        state1.started_at = Some(Utc::now() - chrono::Duration::hours(1));
        store.save_workflow_state(&id1, &state1).await.unwrap();

        let mut state2 = WorkflowState::new("s1".into());
        state2.started_at = Some(Utc::now());
        store.save_workflow_state(&id2, &state2).await.unwrap();

        let tasks = store.list_tasks().await.unwrap();
        assert_eq!(tasks.len(), 2);
        // Most recent first
        assert_eq!(tasks[0].task_id.0, "task-new");
        assert_eq!(tasks[1].task_id.0, "task-old");
    }
}
