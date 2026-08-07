//! Generic event stream abstraction for event-driven agent wake-ups.
//!
//! `EventStream` is a minimal, object-safe trait with a single `next()`
//! method. Business layers implement it for any event source (timers,
//! IO completion, upstream task completion, external signals). The
//! runtime's `WaitForExternalEventTool` blocks on `stream.next().await`;
//! when an event arrives, the tool returns and the agent loop continues.
//!
//! # Design
//!
//! - `Send` but not `Sync` — holds mutable wait state (e.g.
//!   `mpsc::Receiver`), single-consumer.
//! - Uses `BoxFuture` instead of `async fn` to keep the trait
//!   object-safe (`Box<dyn EventStream>` works), matching the
//!   `ShouldStopHook` / `Tool` signature style in this codebase.
//! - Crash recovery is the business layer's responsibility: the runtime
//!   provides `TaskStore::save_event_wait` / `load_event_wait` for
//!   persisting a wait descriptor, but does not interpret it.

use futures::future::BoxFuture;
use serde_json::Value;

use crate::DataBlock;

/// Generic event stream trait. Business layers implement this.
///
/// Sources: timers, IO completion, upstream task completion, external
/// signals — anything that can wake up an agent waiting in
/// `WaitForExternalEventTool`.
///
/// `Send` but not `Sync` — holds mutable wait state (e.g.
/// `mpsc::Receiver`), single-consumer.
pub trait EventStream: Send {
    /// Block until the next event is available.
    /// Returns `None` when the stream has ended (no more events).
    fn next<'a>(&'a mut self) -> BoxFuture<'a, Option<Event>>;
}

/// Event payload. `content` is for the LLM, `details` is for programmatic
/// consumption.
#[derive(Debug, Clone)]
pub struct Event {
    /// Isomorphic with `ToolResult.model_content`.
    pub content: Vec<DataBlock>,
    /// Structured data for programmatic consumption.
    pub details: Value,
}
