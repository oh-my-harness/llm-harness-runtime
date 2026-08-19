//! Root-confined Markdown and text implementation of `KnowledgeSource`.

mod fs;
mod index;
mod parser;
mod revision;
mod source;

pub use fs::*;
pub use index::*;
pub use parser::*;
pub use revision::*;
pub use source::*;
