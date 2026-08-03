//! Opt-in, runtime-owned cross-Session history recall.
//!
//! Session authority remains in `SessionRepo`; indexes in this crate are
//! disposable projections and exact reads always reopen current authority.
//!
//! Wiring is explicit: create an index and [`SessionRecallProjector`], wrap the
//! authoritative repo with `ObservedSessionRepo`, register
//! [`SessionRecallKnowledgeSource`] in the common Knowledge registry, and add
//! [`HistoryRecallPlugin`] to the harness. Tool-driven reads require trusted
//! `KnowledgeAccessContext` and [`SessionRecallAccessContext`] extensions; the
//! automatic plugin additionally requires [`HistoryRecallRequest`]. Without
//! the wrapper, source, and extensions, existing Session behavior is unchanged
//! and no indexing or background work exists.

mod index;
mod plugin;
mod projector;
mod source;
mod types;

pub use index::*;
pub use plugin::*;
pub use projector::*;
pub use source::*;
pub use types::*;
