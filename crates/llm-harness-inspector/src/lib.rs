//! Agent Inspector — Web API for debugging agent state and events.

#[cfg(feature = "inspect")]
pub mod event_ser;
#[cfg(feature = "inspect")]
pub mod routes;
#[cfg(feature = "inspect")]
pub mod server;
#[cfg(feature = "inspect")]
pub mod state;
#[cfg(feature = "inspect")]
mod static_files;

#[cfg(feature = "inspect")]
pub use server::{Inspector, InspectorBuilder, InspectorError};
