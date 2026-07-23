use std::path::{Path, PathBuf};

use futures::future::BoxFuture;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentMetadata {
    pub is_file: bool,
    pub is_dir: bool,
    pub len: u64,
}

#[derive(thiserror::Error, Debug)]
pub enum DocumentFileSystemError {
    #[error("document filesystem operation failed")]
    Io(#[source] std::io::Error),
}

impl From<std::io::Error> for DocumentFileSystemError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Minimal read-only filesystem surface required by `LocalDocumentSource`.
pub trait DocumentFileSystem: Send + Sync {
    fn canonicalize<'a>(
        &'a self,
        path: &'a Path,
    ) -> BoxFuture<'a, Result<PathBuf, DocumentFileSystemError>>;

    fn list_files<'a>(
        &'a self,
        root: &'a Path,
    ) -> BoxFuture<'a, Result<Vec<PathBuf>, DocumentFileSystemError>>;

    fn metadata<'a>(
        &'a self,
        path: &'a Path,
    ) -> BoxFuture<'a, Result<DocumentMetadata, DocumentFileSystemError>>;

    fn read<'a>(
        &'a self,
        path: &'a Path,
    ) -> BoxFuture<'a, Result<Vec<u8>, DocumentFileSystemError>>;
}

#[derive(Debug, Default)]
pub struct OsDocumentFileSystem;

impl DocumentFileSystem for OsDocumentFileSystem {
    fn canonicalize<'a>(
        &'a self,
        path: &'a Path,
    ) -> BoxFuture<'a, Result<PathBuf, DocumentFileSystemError>> {
        Box::pin(async move { tokio::fs::canonicalize(path).await.map_err(Into::into) })
    }

    fn list_files<'a>(
        &'a self,
        root: &'a Path,
    ) -> BoxFuture<'a, Result<Vec<PathBuf>, DocumentFileSystemError>> {
        Box::pin(async move {
            let mut files = Vec::new();
            let mut pending = vec![root.to_path_buf()];
            while let Some(directory) = pending.pop() {
                let mut entries = tokio::fs::read_dir(&directory).await?;
                while let Some(entry) = entries.next_entry().await? {
                    let file_type = entry.file_type().await?;
                    if file_type.is_dir() {
                        pending.push(entry.path());
                    } else {
                        files.push(entry.path());
                    }
                }
            }
            Ok(files)
        })
    }

    fn metadata<'a>(
        &'a self,
        path: &'a Path,
    ) -> BoxFuture<'a, Result<DocumentMetadata, DocumentFileSystemError>> {
        Box::pin(async move {
            let metadata = tokio::fs::metadata(path).await?;
            Ok(DocumentMetadata {
                is_file: metadata.is_file(),
                is_dir: metadata.is_dir(),
                len: metadata.len(),
            })
        })
    }

    fn read<'a>(
        &'a self,
        path: &'a Path,
    ) -> BoxFuture<'a, Result<Vec<u8>, DocumentFileSystemError>> {
        Box::pin(async move { tokio::fs::read(path).await.map_err(Into::into) })
    }
}
