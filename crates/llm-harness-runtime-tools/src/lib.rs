//! Out-of-the-box basic tools (`bash`, `read`, `write`, `edit`) for
//! `llm-harness-runtime`-based agents.
//!
//! All tools map onto [`llm_harness_types::ExecutionEnv`] primitives.
//! `read` and `edit` are coupled via [`FileSnapshotStore`] using
//! hashline anchors (4-hex content tags) to detect stale edits.

pub mod bash;
pub mod edit;
pub mod read;
pub mod snapshot;
#[cfg(test)]
pub mod test_env;
pub mod truncation;
pub mod write;

pub use bash::BashTool;
pub use edit::{EditOperation, EditTool};
pub use read::ReadTool;
pub use snapshot::FileSnapshotStore;
pub use write::WriteTool;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use llm_harness_agent::Plugin;
use llm_harness_types::Tool;
use parking_lot::RwLock;

/// Resolve a path against the env working dir if relative.
pub(crate) fn resolve_path(working_dir: &Path, p: &str) -> PathBuf {
    let path = PathBuf::from(p);
    if path.is_absolute() {
        path
    } else {
        working_dir.join(path)
    }
}

/// Plugin that registers `bash`, `read`, `write`, `edit` tools with an
/// optional shared `FileSnapshotStore` coupling `read` and `edit`.
pub struct FsToolsPlugin {
    store: Option<Arc<RwLock<FileSnapshotStore>>>,
}

impl FsToolsPlugin {
    /// Create a new `FsToolsPlugin` with an optional shared snapshot store.
    pub fn new(store: Option<Arc<RwLock<FileSnapshotStore>>>) -> Self {
        Self { store }
    }
}

impl Plugin for FsToolsPlugin {
    fn name(&self) -> &str {
        "fs-tools"
    }

    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        tools.push(Arc::new(ReadTool::new(self.store.clone())));
        tools.push(Arc::new(WriteTool::new(self.store.clone())));
        tools.push(Arc::new(EditTool::new(self.store.clone())));
        tools.push(Arc::new(BashTool::new()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::Tool;

    #[test]
    fn plugin_registers_four_tools() {
        let plugin = FsToolsPlugin::new(None);
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        plugin.register_tools(&mut tools);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"read"));
        assert!(names.contains(&"write"));
        assert!(names.contains(&"edit"));
        assert!(names.contains(&"bash"));
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn plugin_name() {
        let plugin = FsToolsPlugin::new(None);
        assert_eq!(plugin.name(), "fs-tools");
    }
}
