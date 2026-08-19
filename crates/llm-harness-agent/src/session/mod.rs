//! # Session module — persistent conversation history with branching
//!
//! ## Three-layer architecture
//!
//! | Layer | Type | Responsibility |
//! |-------|------|----------------|
//! | **Repo** | [`SessionRepo`] | Manages multiple sessions: create, open, list, delete, fork. Factory for `Session` instances. |
//! | **Session** | [`Session`] | Single-session high-level API: append messages, build LLM context, branch/fork/navigate. This is the primary interface for most callers. |
//! | **Storage** | [`SessionStorage`] | Low-level backend trait: entry CRUD, tree traversal, cursor management. Implementations are `pub(crate)` — third parties cannot provide custom backends (yet). |
//!
//! Typical call chain: `SessionRepo::create/open` → `Arc<dyn SessionStorage>` → `Session::new(storage)` → use `Session` methods.
//! `AgentHarness` handles this wiring internally; downstream code rarely constructs `Session` directly.
//!
//! ## Choosing a repo
//!
//! | Repo | When to use |
//! |------|-------------|
//! | [`InMemorySessionRepo`] | Tests, ephemeral sessions. Lost on process exit. No recall support. |
//! | [`JsonlSessionRepo`] | Production. Persists to disk (`meta.json` + `entries.jsonl`). Supports recall. |
//! | [`ObservedSessionRepo`] | Wrapper around any `SessionRepo` that emits [`SessionMutation`] events to a [`SessionMutationObserver`]. Required when cross-session recall indexing is enabled. |
//!
//! For recall: wrap `JsonlSessionRepo` with `ObservedSessionRepo`, install a `SessionMutationObserver`
//! (e.g. `SessionRecallProjector` from `llm-harness-session-recall`), and the rest is automatic.
//! Without the wrapper, `update_recall_settings` on `InMemorySessionStorage` returns `RecallMetadataUnsupported`.
//!
//! ## Entry tree model
//!
//! Messages are stored as a tree, not a flat list. Each [`SessionEntry`] has a `parent` pointer.
//! The **active cursor** marks the current leaf; [`Session::build_context`] returns only the path
//! from root to the active cursor. Forking creates a new branch from any node; navigating switches
//! the active cursor to a different leaf without deleting anything.
//!
//! See [`Session`] docs for details on append, fork, and navigation semantics.
//!
//! ## Concurrency
//!
//! Individual storage methods are thread-safe. However, composite read-modify-write sequences
//! (e.g. `active_cursor` → `append_entry` → `set_active_cursor` inside [`Session::append`]) are
//! **not atomic**. `AgentHarness` serializes these via its phase guard (`phase != Idle` rejects
//! concurrent writes). If using `Session` outside `AgentHarness`, the caller must provide their
//! own serialization.
#![allow(clippy::module_inception)]
pub mod jsonl;
pub mod projection;
pub mod repo;
pub mod session;
pub mod storage;
pub(crate) mod tree_ops;
pub mod types;

pub use jsonl::JsonlSessionRepo;
pub use projection::{
    ObservedSessionRepo, SessionMutation, SessionMutationKind, SessionMutationObserver,
};
pub use repo::{InMemorySessionRepo, SessionRepo};
pub use session::Session;
pub use storage::SessionStorage;
pub use types::*;
