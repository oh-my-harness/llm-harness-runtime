//! Embeds the viewer HTML. Kept in a separate file for editability.
//!
//! The HTML page reads its data from a `<script id="viewer-data">` tag whose
//! body is the JSON string placeholder `__VIEWER_DATA_JSON__` (replaced at
//! serve time by [`crate::render_page`]).

/// The full self-contained HTML page. Contains `__VIEWER_DATA_JSON__` placeholder.
pub const VIEWER_HTML: &str = include_str!("../static/viewer.html");
