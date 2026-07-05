//! Task persistence and event-driven resumption.
//!
//! `task` holds plain data types (`TaskId`, `TaskResult`, `Checkpoint`);
//! `task_store` is the persistence trait + JSONL reference impl;
//! `event` provides the generic `EventStream` trait and a blocking wait tool.

pub mod event;
pub mod task;
pub mod task_store;
