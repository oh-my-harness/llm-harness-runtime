//! Stable, backend-neutral contracts for read-only knowledge access.
//!
//! This crate defines what a knowledge source can expose and how trusted
//! per-run identity is authorized. It deliberately does not choose a search
//! engine, document store, router, tool surface, or memory implementation.

pub mod access;
pub mod source;

#[cfg(any(test, feature = "test-utils"))]
pub mod contract;

pub use access::*;
pub use source::*;
