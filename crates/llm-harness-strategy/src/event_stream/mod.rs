//! Built-in EventStream implementations: timer, heartbeat, filter wrapper,
//! the default event payload template, shell process monitor, and webhook
//! receiver.
//!
//! These are strategy-layer defaults for the `EventStream` trait (defined
//! in `llm-harness-types`). They live in the strategy crate because they
//! are optional, user-selectable implementations — the runtime crate's
//! `WaitForExternalEventTool` consumes `Box<dyn EventStream>` without
//! knowing the concrete type.

pub mod filter;
pub use filter::{FilterStream, by_channel, by_field, by_source, pass_all};

pub mod heartbeat;
pub use heartbeat::{HeartbeatHandle, HeartbeatStream};

pub mod template;
pub use template::{EventContext, EventDetails};

pub mod timer;
pub use timer::{TimerKind, TimerStream};

pub mod shell_monitor;
pub use shell_monitor::{ShellMonitorConfig, ShellMonitorHandle, ShellMonitorStream};

pub mod webhook;
pub use webhook::{WebhookChannel, WebhookStream};
