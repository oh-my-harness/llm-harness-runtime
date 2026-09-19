//! Linux guest workspace filesystem handler.
//!
//! The handler keeps one `O_PATH|O_DIRECTORY|O_NOFOLLOW` descriptor for the
//! workspace root and resolves every request component with `openat`. This
//! avoids path-string canonicalization races and prevents a workload from
//! replacing a parent directory or final file with a symlink between validation
//! and use.

use std::collections::HashSet;
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine;
use llm_harness_vm_agent_protocol::{AgentMessage, FileContent, FileEntry};
use rustix::fd::AsFd;
use rustix::fd::OwnedFd;
use rustix::fs::{self, AtFlags, Dir, FileType, Mode, OFlags, ResolveFlags};
use uuid::Uuid;

use crate::{GuestAgentHandler, HandlerError};

const MAX_RECURSIVE_REMOVE_DEPTH: usize = 128;
const MAX_WORKSPACE_PATH_BYTES: usize = 4096;
const MAX_WORKSPACE_PATH_COMPONENTS: usize = 256;
const MAX_TEXT_FILE_TRANSFER_BYTES: usize = 8 * 1024 * 1024;
const MAX_BINARY_FILE_TRANSFER_BYTES: usize = 32 * 1024 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 10_000;
const MAX_DIRECTORY_LISTING_PATH_BYTES: usize = 8 * 1024 * 1024;
const MAX_TEMP_DIR_PREFIX_BYTES: usize = 128;

const WORKSPACE_RESOLVE_FLAGS: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_XDEV);

/// A production guest filesystem handler rooted at a Linux directory.
pub struct WorkspaceFileHandler {
    root_fd: OwnedFd,
    temp_dirs: Arc<Mutex<HashSet<String>>>,
}

impl WorkspaceFileHandler {
    /// Open a workspace root without following a final symlink.
    pub fn new(root: impl AsRef<std::path::Path>) -> Result<Self, HandlerError> {
        let root_fd = fs::open(
            root.as_ref(),
            OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| filesystem_error("open workspace root", error))?;
        Ok(Self {
            root_fd,
            temp_dirs: Arc::default(),
        })
    }
}

#[async_trait]
impl GuestAgentHandler for WorkspaceFileHandler {
    async fn handle(&self, request: AgentMessage) -> Result<AgentMessage, HandlerError> {
        request
            .validate()
            .map_err(|error| HandlerError::operation("protocol_error", error.to_string()))?;

        let root_fd = self
            .root_fd
            .try_clone()
            .map_err(|error| filesystem_error("clone workspace root", error))?;
        let temp_dirs = Arc::clone(&self.temp_dirs);
        tokio::task::spawn_blocking(move || handle_request(root_fd, request, temp_dirs))
            .await
            .map_err(|error| HandlerError::operation("handler_failure", error.to_string()))?
    }
}

fn handle_request(
    root_fd: OwnedFd,
    request: AgentMessage,
    temp_dirs: Arc<Mutex<HashSet<String>>>,
) -> Result<AgentMessage, HandlerError> {
    match request {
        AgentMessage::Ping => Ok(AgentMessage::Pong),
        AgentMessage::Status => Ok(AgentMessage::StatusResult {
            agent_version: env!("CARGO_PKG_VERSION").to_owned(),
            active_processes: 0,
        }),
        AgentMessage::ReadFile { path, binary } => read_file(&root_fd, &path, binary),
        AgentMessage::WriteFile {
            path,
            content,
            append,
        } => write_file(&root_fd, &path, content, append),
        AgentMessage::FileInfo { path } => file_info(&root_fd, &path),
        AgentMessage::ListDir { path } => list_dir(&root_fd, &path),
        AgentMessage::Exists { path } => exists(&root_fd, &path),
        AgentMessage::CreateDir { path, recursive } => create_dir(&root_fd, &path, recursive),
        AgentMessage::Remove {
            path,
            recursive,
            force,
        } => remove_path(&root_fd, &path, recursive, force),
        AgentMessage::CreateTempDir { prefix } => create_temp_dir(&root_fd, &prefix, &temp_dirs),
        AgentMessage::Cleanup => cleanup_temp_dirs(&root_fd, &temp_dirs),
        AgentMessage::StartProcess { .. } => Err(HandlerError::unsupported("start_process")),
        AgentMessage::CancelProcess { .. } => Err(HandlerError::unsupported("cancel_process")),
        _ => Err(HandlerError::unsupported("request")),
    }
}

fn workspace_components(path: &str) -> Result<Vec<CString>, HandlerError> {
    if path.len() > MAX_WORKSPACE_PATH_BYTES {
        return Err(invalid_path("workspace path is too long"));
    }
    if path == "/workspace" {
        return Ok(Vec::new());
    }
    let relative = path
        .strip_prefix("/workspace/")
        .ok_or_else(|| invalid_path("path must be below /workspace"))?;
    if relative.is_empty() {
        return Err(invalid_path("path component is empty"));
    }

    let mut components = Vec::new();
    for component in relative.split('/') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.contains('\\')
            || component.contains('\0')
        {
            return Err(invalid_path("path contains a forbidden component"));
        }
        components.push(
            CString::new(component.as_bytes())
                .map_err(|_| invalid_path("path contains a NUL byte"))?,
        );
    }
    if components.len() > MAX_WORKSPACE_PATH_COMPONENTS {
        return Err(invalid_path("workspace path has too many components"));
    }
    Ok(components)
}

fn open_regular_file(
    parent: &OwnedFd,
    name: &CString,
    mut flags: OFlags,
) -> Result<OwnedFd, HandlerError> {
    match fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile => {
            return Err(HandlerError::operation(
                "filesystem_error",
                "workspace file target is not a regular file",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(filesystem_error("read file metadata", error)),
    }

    flags |= OFlags::NONBLOCK | OFlags::NOCTTY;
    let mode = if flags.contains(OFlags::CREATE) {
        Mode::RUSR | Mode::WUSR
    } else {
        Mode::empty()
    };
    let file_fd = open_workspace_at(parent, name, flags, mode)
        .map_err(|error| filesystem_error("open file", error))?;
    let stat = fs::fstat(&file_fd)
        .map_err(|error| filesystem_error("read opened file metadata", error))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(HandlerError::operation(
            "filesystem_error",
            "workspace file target is not a regular file",
        ));
    }
    Ok(file_fd)
}

fn open_workspace_at<Fd: AsFd>(
    parent: &Fd,
    name: &std::ffi::CStr,
    flags: OFlags,
    mode: Mode,
) -> io::Result<OwnedFd> {
    match fs::openat2(parent, name, flags, mode, WORKSPACE_RESOLVE_FLAGS) {
        Ok(file_fd) => Ok(file_fd),
        Err(error) if error.kind() == io::ErrorKind::Unsupported => {
            fs::openat(parent, name, flags, mode).map_err(io::Error::from)
        }
        Err(error) => Err(io::Error::from(error)),
    }
}

fn open_parent(
    root_fd: &OwnedFd,
    components: &[CString],
) -> Result<(OwnedFd, CString), HandlerError> {
    let (name, parents) = components
        .split_last()
        .ok_or_else(|| invalid_path("workspace root cannot be a file target"))?;
    let parent = open_dir(root_fd, parents)?;
    Ok((parent, name.clone()))
}

fn open_dir(root_fd: &OwnedFd, components: &[CString]) -> Result<OwnedFd, HandlerError> {
    let mut current = root_fd
        .try_clone()
        .map_err(|error| filesystem_error("clone directory descriptor", error))?;
    for component in components {
        let next = open_workspace_at(
            &current,
            component,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| filesystem_error("open workspace directory", error))?;
        current = next;
    }
    Ok(current)
}

fn read_file(root_fd: &OwnedFd, path: &str, binary: bool) -> Result<AgentMessage, HandlerError> {
    let components = workspace_components(path)?;
    let (parent, name) = open_parent(root_fd, &components)?;
    let file_fd = open_regular_file(
        &parent,
        &name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
    )?;
    let file = File::from(file_fd);
    let mut bytes = Vec::new();
    let transfer_limit = if binary {
        MAX_BINARY_FILE_TRANSFER_BYTES
    } else {
        MAX_TEXT_FILE_TRANSFER_BYTES
    };
    file.take((transfer_limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| filesystem_error("read file", error))?;
    if bytes.len() > transfer_limit {
        return Err(HandlerError::operation(
            "filesystem_error",
            "file exceeds the guest transfer limit",
        ));
    }

    let content = if binary {
        FileContent::Binary {
            base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    } else {
        if bytes.contains(&0) {
            return Err(HandlerError::operation(
                "filesystem_error",
                "text file contains a NUL byte",
            ));
        }
        let value = String::from_utf8(bytes)
            .map_err(|_| HandlerError::operation("filesystem_error", "file is not valid UTF-8"))?;
        FileContent::Text { value }
    };
    Ok(AgentMessage::ReadFileResult { content })
}

fn write_file(
    root_fd: &OwnedFd,
    path: &str,
    content: FileContent,
    append: bool,
) -> Result<AgentMessage, HandlerError> {
    let components = workspace_components(path)?;
    let (parent, name) = open_parent(root_fd, &components)?;
    let bytes = match content {
        FileContent::Text { value } => {
            let bytes = value.into_bytes();
            if bytes.len() > MAX_TEXT_FILE_TRANSFER_BYTES {
                return Err(HandlerError::operation(
                    "filesystem_error",
                    "file content exceeds the text transfer limit",
                ));
            }
            bytes
        }
        FileContent::Binary { base64 } => {
            if base64.len() / 4 * 3 > MAX_BINARY_FILE_TRANSFER_BYTES {
                return Err(HandlerError::operation(
                    "filesystem_error",
                    "file content exceeds the binary transfer limit",
                ));
            }
            base64::engine::general_purpose::STANDARD
                .decode(base64)
                .map_err(|_| HandlerError::operation("protocol_error", "invalid base64 content"))?
        }
    };
    let mut flags = OFlags::WRONLY | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    flags |= if append {
        OFlags::APPEND
    } else {
        OFlags::TRUNC
    };
    let file_fd = open_regular_file(&parent, &name, flags)?;
    let mut file = File::from(file_fd);
    file.write_all(&bytes)
        .map_err(|error| filesystem_error("write file", error))?;
    Ok(AgentMessage::WriteFileResult)
}

fn file_info(root_fd: &OwnedFd, path: &str) -> Result<AgentMessage, HandlerError> {
    let components = workspace_components(path)?;
    let stat = if components.is_empty() {
        fs::fstat(root_fd).map_err(|error| filesystem_error("read workspace metadata", error))?
    } else {
        let (parent, name) = open_parent(root_fd, &components)?;
        fs::statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| filesystem_error("read file metadata", error))?
    };
    Ok(AgentMessage::FileInfoResult {
        entry: file_entry(path, &stat),
    })
}

fn list_dir(root_fd: &OwnedFd, path: &str) -> Result<AgentMessage, HandlerError> {
    let components = workspace_components(path)?;
    let directory_fd = if components.is_empty() {
        let root_name = CString::new(".").expect("valid root directory name");
        open_workspace_at(
            root_fd,
            &root_name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| filesystem_error("open workspace directory", error))?
    } else {
        let (parent, name) = open_parent(root_fd, &components)?;
        open_workspace_at(
            &parent,
            &name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| filesystem_error("open directory", error))?
    };

    let mut directory =
        Dir::new(directory_fd).map_err(|error| filesystem_error("iterate directory", error))?;
    let mut entries = Vec::new();
    let mut path_bytes = 0_usize;
    loop {
        let entry = match directory.read() {
            Some(Ok(entry)) => entry,
            Some(Err(error)) => {
                return Err(filesystem_error("iterate directory", error));
            }
            None => break,
        };
        let name = entry.file_name().to_str().map_err(|_| {
            HandlerError::operation(
                "filesystem_error",
                "directory entry name is not valid UTF-8",
            )
        })?;
        if name == "." || name == ".." {
            continue;
        }
        let directory_fd = directory
            .fd()
            .map_err(|error| filesystem_error("iterate directory", error))?;
        let stat = fs::statat(directory_fd, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| filesystem_error("read directory entry metadata", error))?;
        let entry_path = format!("{path}/{name}");
        if !is_representable_path(&entry_path) {
            return Err(HandlerError::operation(
                "filesystem_error",
                "directory entry cannot be represented by the guest protocol",
            ));
        }
        if entries.len() >= MAX_DIRECTORY_ENTRIES {
            return Err(HandlerError::operation(
                "filesystem_error",
                "directory exceeds the guest listing limit",
            ));
        }
        path_bytes += entry_path.len();
        if path_bytes > MAX_DIRECTORY_LISTING_PATH_BYTES {
            return Err(HandlerError::operation(
                "filesystem_error",
                "directory listing exceeds the guest response limit",
            ));
        }
        entries.push(file_entry(&entry_path, &stat));
    }
    Ok(AgentMessage::ListDirResult { entries })
}

fn exists(root_fd: &OwnedFd, path: &str) -> Result<AgentMessage, HandlerError> {
    let components = workspace_components(path)?;
    if components.is_empty() {
        return Ok(AgentMessage::ExistsResult { exists: true });
    }
    let (parent, name) = open_parent(root_fd, &components)?;
    match fs::statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(AgentMessage::ExistsResult { exists: true }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok(AgentMessage::ExistsResult { exists: false })
        }
        Err(error) => Err(filesystem_error("check path existence", error)),
    }
}

fn create_dir(
    root_fd: &OwnedFd,
    path: &str,
    recursive: bool,
) -> Result<AgentMessage, HandlerError> {
    let components = workspace_components(path)?;
    if components.is_empty() {
        return Err(HandlerError::operation(
            "filesystem_error",
            "workspace root already exists",
        ));
    }

    if recursive {
        let mut current = root_fd
            .try_clone()
            .map_err(|error| filesystem_error("clone workspace root", error))?;
        for component in &components {
            match fs::mkdirat(&current, component, Mode::RWXU) {
                Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
                    return Err(filesystem_error("create directory", error));
                }
                _ => {}
            }
            current = open_workspace_at(
                &current,
                component,
                OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| filesystem_error("open created directory", error))?;
        }
    } else {
        let (parent, name) = open_parent(root_fd, &components)?;
        fs::mkdirat(&parent, &name, Mode::RWXU)
            .map_err(|error| filesystem_error("create directory", error))?;
    }
    Ok(AgentMessage::CreateDirResult)
}

fn remove_path(
    root_fd: &OwnedFd,
    path: &str,
    recursive: bool,
    force: bool,
) -> Result<AgentMessage, HandlerError> {
    let components = workspace_components(path)?;
    if components.is_empty() {
        return Err(HandlerError::operation(
            "filesystem_error",
            "workspace root cannot be removed",
        ));
    }
    let (parent, name) = open_parent(root_fd, &components)?;
    match remove_entry(&parent, &name, recursive, 0) {
        Ok(()) => Ok(AgentMessage::RemoveResult),
        Err(error) if force && error.kind() == io::ErrorKind::NotFound => {
            Ok(AgentMessage::RemoveResult)
        }
        Err(error) => Err(filesystem_error("remove path", error)),
    }
}

fn create_temp_dir(
    root_fd: &OwnedFd,
    prefix: &str,
    temp_dirs: &Mutex<HashSet<String>>,
) -> Result<AgentMessage, HandlerError> {
    if prefix.len() > MAX_TEMP_DIR_PREFIX_BYTES {
        return Err(HandlerError::operation(
            "filesystem_error",
            "temporary directory prefix is too long",
        ));
    }
    let name = format!("{prefix}-{}", Uuid::new_v4());
    let c_name = CString::new(name.as_bytes())
        .map_err(|_| invalid_path("temporary directory name contains a NUL byte"))?;
    fs::mkdirat(root_fd, &c_name, Mode::RWXU)
        .map_err(|error| filesystem_error("create temporary directory", error))?;
    let path = format!("/workspace/{name}");
    temp_dirs
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(path.clone());
    Ok(AgentMessage::CreateTempDirResult { path })
}

fn cleanup_temp_dirs(
    root_fd: &OwnedFd,
    temp_dirs: &Mutex<HashSet<String>>,
) -> Result<AgentMessage, HandlerError> {
    let paths = temp_dirs
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    for path in paths {
        remove_path(root_fd, &path, true, true)?;
        temp_dirs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&path);
    }
    Ok(AgentMessage::CleanupResult)
}

fn remove_entry<Fd: AsFd>(
    parent_fd: Fd,
    name: &std::ffi::CStr,
    recursive: bool,
    depth: usize,
) -> io::Result<()> {
    let stat = fs::statat(&parent_fd, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
        return fs::unlinkat(parent_fd, name, AtFlags::empty()).map_err(io::Error::from);
    }
    if !recursive {
        return fs::unlinkat(&parent_fd, name, AtFlags::REMOVEDIR).map_err(io::Error::from);
    }
    if depth >= MAX_RECURSIVE_REMOVE_DEPTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "recursive remove depth exceeded",
        ));
    }

    let child_fd = open_workspace_at(
        &parent_fd,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut directory = Dir::new(child_fd)?;
    while let Some(entry) = directory.read() {
        let entry = entry?;
        let entry_name = entry.file_name();
        if entry_name.to_bytes() == b"." || entry_name.to_bytes() == b".." {
            continue;
        }
        let child_parent_fd = directory.fd().map_err(io::Error::from)?;
        remove_entry(child_parent_fd, entry_name, true, depth + 1)?;
    }
    fs::unlinkat(&parent_fd, name, AtFlags::REMOVEDIR).map_err(io::Error::from)
}

fn file_entry(path: &str, stat: &fs::Stat) -> FileEntry {
    FileEntry {
        path: path.to_owned(),
        is_dir: FileType::from_raw_mode(stat.st_mode) == FileType::Directory,
        size: u64::try_from(stat.st_size).unwrap_or_default(),
        modified_ms: modified_ms(stat),
    }
}

fn modified_ms(stat: &fs::Stat) -> u64 {
    if stat.st_mtime < 0 {
        return 0;
    }
    let seconds = u64::try_from(stat.st_mtime).unwrap_or_default();
    seconds
        .saturating_mul(1_000)
        .saturating_add(stat.st_mtime_nsec / 1_000_000)
}

fn is_representable_path(path: &str) -> bool {
    path.starts_with("/workspace/")
        && !path.ends_with('/')
        && !path.contains("//")
        && !path.contains('\\')
        && !path.contains('\0')
        && !path
            .split('/')
            .any(|component| component == "." || component == "..")
}

fn filesystem_error(operation: &'static str, error: impl std::fmt::Display) -> HandlerError {
    HandlerError::operation("filesystem_error", format!("{operation} failed: {error}"))
}

fn invalid_path(message: &'static str) -> HandlerError {
    HandlerError::operation("filesystem_error", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GuestAgentService;
    use llm_harness_vm_agent_protocol::{Frame, ProtocolPeer};
    use llm_harness_vm_agent_transport::FrameStream;
    use tokio::io::duplex;
    use tokio::spawn;

    fn handler(root: &std::path::Path) -> WorkspaceFileHandler {
        WorkspaceFileHandler::new(root).expect("workspace handler")
    }

    fn text(value: &str) -> FileContent {
        FileContent::Text {
            value: value.to_owned(),
        }
    }

    #[tokio::test]
    async fn writes_reads_appends_text_and_binary_files() {
        let root = tempfile::tempdir().expect("root");
        let handler = handler(root.path());

        handler
            .handle(AgentMessage::WriteFile {
                path: "/workspace/file.txt".to_owned(),
                content: text("first"),
                append: false,
            })
            .await
            .expect("write");
        handler
            .handle(AgentMessage::WriteFile {
                path: "/workspace/file.txt".to_owned(),
                content: text("-second"),
                append: true,
            })
            .await
            .expect("append");
        match handler
            .handle(AgentMessage::ReadFile {
                path: "/workspace/file.txt".to_owned(),
                binary: false,
            })
            .await
            .expect("read text")
        {
            AgentMessage::ReadFileResult { content } => {
                assert_eq!(content, text("first-second"));
            }
            message => panic!("unexpected response: {message:?}"),
        }

        handler
            .handle(AgentMessage::WriteFile {
                path: "/workspace/file.bin".to_owned(),
                content: FileContent::Binary {
                    base64: base64::engine::general_purpose::STANDARD.encode([0, 255, 10]),
                },
                append: false,
            })
            .await
            .expect("write binary");
        match handler
            .handle(AgentMessage::ReadFile {
                path: "/workspace/file.bin".to_owned(),
                binary: true,
            })
            .await
            .expect("read binary")
        {
            AgentMessage::ReadFileResult { content } => {
                let FileContent::Binary { base64 } = content else {
                    panic!("expected binary content");
                };
                assert_eq!(
                    base64::engine::general_purpose::STANDARD
                        .decode(base64)
                        .unwrap(),
                    vec![0, 255, 10]
                );
            }
            message => panic!("unexpected response: {message:?}"),
        }
    }

    #[tokio::test]
    async fn metadata_listing_exists_and_remove_are_supported() {
        let root = tempfile::tempdir().expect("root");
        let handler = handler(root.path());

        handler
            .handle(AgentMessage::CreateDir {
                path: "/workspace/dir".to_owned(),
                recursive: false,
            })
            .await
            .expect("create dir");
        handler
            .handle(AgentMessage::WriteFile {
                path: "/workspace/dir/file".to_owned(),
                content: text("data"),
                append: false,
            })
            .await
            .expect("write file");

        match handler
            .handle(AgentMessage::FileInfo {
                path: "/workspace/dir/file".to_owned(),
            })
            .await
            .expect("file info")
        {
            AgentMessage::FileInfoResult { entry } => {
                assert_eq!(entry.path, "/workspace/dir/file");
                assert_eq!(entry.size, 4);
                assert!(!entry.is_dir);
            }
            message => panic!("unexpected response: {message:?}"),
        }
        match handler
            .handle(AgentMessage::ListDir {
                path: "/workspace".to_owned(),
            })
            .await
            .expect("list workspace root")
        {
            AgentMessage::ListDirResult { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].path, "/workspace/dir");
            }
            message => panic!("unexpected response: {message:?}"),
        }
        match handler
            .handle(AgentMessage::ListDir {
                path: "/workspace/dir".to_owned(),
            })
            .await
            .expect("list dir")
        {
            AgentMessage::ListDirResult { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].path, "/workspace/dir/file");
            }
            message => panic!("unexpected response: {message:?}"),
        }
        match handler
            .handle(AgentMessage::Exists {
                path: "/workspace/dir/file".to_owned(),
            })
            .await
            .expect("exists")
        {
            AgentMessage::ExistsResult { exists } => assert!(exists),
            message => panic!("unexpected response: {message:?}"),
        }

        handler
            .handle(AgentMessage::Remove {
                path: "/workspace/dir".to_owned(),
                recursive: true,
                force: false,
            })
            .await
            .expect("remove");
        assert!(!root.path().join("dir").exists());
    }

    #[tokio::test]
    async fn cleanup_removes_only_tracked_temp_directories() {
        let root = tempfile::tempdir().expect("root");
        let handler = handler(root.path());
        std::fs::write(root.path().join("user-file"), b"keep").expect("write user file");

        let temp_path = match handler
            .handle(AgentMessage::CreateTempDir {
                prefix: "agent".to_owned(),
            })
            .await
            .expect("create temp")
        {
            AgentMessage::CreateTempDirResult { path } => path,
            message => panic!("unexpected response: {message:?}"),
        };
        let relative = temp_path.strip_prefix("/workspace/").unwrap();
        std::fs::write(root.path().join(relative).join("temporary"), b"temporary")
            .expect("write temp file");

        handler
            .handle(AgentMessage::Cleanup)
            .await
            .expect("cleanup");
        assert!(root.path().join("user-file").exists());
        assert!(!root.path().join(relative).exists());
    }

    #[tokio::test]
    async fn final_and_parent_symlinks_are_rejected() {
        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret"), b"secret").expect("write secret");
        std::os::unix::fs::symlink(outside.path().join("secret"), root.path().join("escape"))
            .expect("final symlink");
        std::os::unix::fs::symlink(outside.path(), root.path().join("parent"))
            .expect("parent symlink");
        let handler = handler(root.path());

        for path in ["/workspace/escape", "/workspace/parent/secret"] {
            let error = handler
                .handle(AgentMessage::ReadFile {
                    path: path.to_owned(),
                    binary: false,
                })
                .await
                .expect_err("symlink must be rejected");
            assert_eq!(error.code(), "filesystem_error");
        }
    }

    #[tokio::test]
    async fn special_files_are_rejected_for_read_and_write() {
        let root = tempfile::tempdir().expect("root");
        let handler = handler(root.path());
        fs::mkfifoat(&handler.root_fd, "fifo", Mode::RUSR | Mode::WUSR).expect("create fifo");

        let error = handler
            .handle(AgentMessage::ReadFile {
                path: "/workspace/fifo".to_owned(),
                binary: false,
            })
            .await
            .expect_err("fifo read must be rejected");
        assert_eq!(error.code(), "filesystem_error");

        let error = handler
            .handle(AgentMessage::WriteFile {
                path: "/workspace/fifo".to_owned(),
                content: text("data"),
                append: false,
            })
            .await
            .expect_err("fifo write must be rejected");
        assert_eq!(error.code(), "filesystem_error");
    }

    #[tokio::test]
    async fn workspace_handler_integrates_with_request_response_service() {
        let root = tempfile::tempdir().expect("root");
        let (host_io, guest_io) = duplex(1024);
        let mut host =
            FrameStream::with_default_limit(host_io, ProtocolPeer::Host).expect("host stream");
        let guest =
            FrameStream::with_default_limit(guest_io, ProtocolPeer::Guest).expect("guest stream");
        let service = GuestAgentService::new(handler(root.path()));
        let service_task = spawn(async move { service.serve(guest).await });

        let write = Frame::new(
            ProtocolPeer::Host,
            91,
            AgentMessage::WriteFile {
                path: "/workspace/integration".to_owned(),
                content: text("ok"),
                append: false,
            },
        )
        .expect("write frame");
        host.send(write).await.expect("send write");
        let response = host.recv().await.expect("write response");
        assert_eq!(response.request_id(), 91);
        assert_eq!(response.into_message(), AgentMessage::WriteFileResult);

        let read = Frame::new(
            ProtocolPeer::Host,
            92,
            AgentMessage::ReadFile {
                path: "/workspace/integration".to_owned(),
                binary: false,
            },
        )
        .expect("read frame");
        host.send(read).await.expect("send read");
        let response = host.recv().await.expect("read response");
        assert_eq!(response.request_id(), 92);
        assert_eq!(
            response.into_message(),
            AgentMessage::ReadFileResult {
                content: text("ok")
            }
        );

        host.close().await.expect("close host");
        drop(host);
        service_task
            .await
            .expect("service completed")
            .expect("service ok");
    }
}
