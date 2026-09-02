//! Task persistence and event-driven resumption.
//!
//! `task` holds plain data types (`TaskId`, `TaskResult`, `Checkpoint`);
//! `task_store` is the persistence trait + JSONL reference impl;
//! `event` provides the generic `EventStream` trait (re-exported from
//! `llm-harness-types`) and a blocking wait tool.
//!
//! Built-in `EventStream` implementations (timer/heartbeat/filter) live
//! in `llm-harness-strategy`.

pub mod event;
pub mod human;
pub mod task;
pub mod task_store;
