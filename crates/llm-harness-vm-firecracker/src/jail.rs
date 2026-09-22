//! Symlink-safe Firecracker jail directory management.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Component, Path};

use crate::image::open_regular_file;

const OPEN_DIRECTORY_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
const CREATE_FILE_FLAGS: libc::c_int =
    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
const MAX_DIRECTORY_DEPTH: usize = 64;
const MAX_DIRECTORY_ENTRIES: usize = 10_000;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct FirecrackerJailError {
    message: String,
}

impl FirecrackerJailError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl From<io::Error> for FirecrackerJailError {
    fn from(error: io::Error) -> Self {
        Self::new(format!("firecracker jail io error: {error}"))
    }
}

impl From<crate::image::FirecrackerImageError> for FirecrackerJailError {
    fn from(error: crate::image::FirecrackerImageError) -> Self {
        Self::new(error.to_string())
    }
}

#[derive(Debug)]
struct CreatedDirectory {
    parent: File,
    name: CString,
    removed: bool,
}

impl CreatedDirectory {
    fn remove(&mut self) -> io::Result<()> {
        if self.removed {
            return Ok(());
        }
        let result = unsafe {
            libc::unlinkat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        };
        if result == 0 {
            self.removed = true;
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            self.removed = true;
            return Ok(());
        }
        Err(error)
    }
}

impl Drop for CreatedDirectory {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

#[derive(Debug)]
pub struct FirecrackerJail {
    root: File,
    root_parent: File,
    root_name: CString,
    created_parents: Vec<CreatedDirectory>,
    removed: bool,
}

impl FirecrackerJail {
    pub fn create(path: &Path) -> Result<Self, FirecrackerJailError> {
        let components = path_components(path)?;
        let Some((last_name, parent_components)) = components.split_last() else {
            return Err(FirecrackerJailError::new("jail root must not be /"));
        };

        let mut current = open_root_directory()?;
        let mut created_parents = Vec::new();
        for name in parent_components {
            match open_directory_at(&current, name) {
                Ok(directory) => current = directory,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let create_result = create_directory_at(&current, name);
                    if let Err(error) = create_result {
                        cleanup_created_directories(&mut created_parents);
                        return Err(error.into());
                    }
                    let parent = match current.try_clone() {
                        Ok(parent) => parent,
                        Err(error) => {
                            let _ = remove_directory_at(&current, name);
                            cleanup_created_directories(&mut created_parents);
                            return Err(error.into());
                        }
                    };
                    let directory = open_directory_at(&current, name).inspect_err(|_| {
                        let _ = remove_directory_at(&current, name);
                        cleanup_created_directories(&mut created_parents);
                    })?;
                    created_parents.push(CreatedDirectory {
                        parent,
                        name: name.clone(),
                        removed: false,
                    });
                    current = directory;
                }
                Err(error) => {
                    cleanup_created_directories(&mut created_parents);
                    return Err(error.into());
                }
            }
        }

        if open_directory_at(&current, last_name).is_ok() {
            cleanup_created_directories(&mut created_parents);
            return Err(FirecrackerJailError::new("jail root already exists"));
        }
        let last_name = last_name.clone();
        if let Err(error) = create_directory_at(&current, &last_name) {
            cleanup_created_directories(&mut created_parents);
            return Err(error.into());
        }
        let root_parent = current;
        let root = match open_directory_at(&root_parent, &last_name) {
            Ok(root) => root,
            Err(error) => {
                let _ = remove_directory_at(&root_parent, &last_name);
                cleanup_created_directories(&mut created_parents);
                return Err(error.into());
            }
        };

        Ok(Self {
            root,
            root_parent,
            root_name: last_name,
            created_parents,
            removed: false,
        })
    }

    pub fn prepare(&self, uid: u32, gid: u32) -> Result<(), FirecrackerJailError> {
        self.root
            .set_permissions(std::fs::Permissions::from_mode(0o700))?;
        chown_file(&self.root, uid, gid)?;
        self.root.sync_all()?;
        Ok(())
    }

    pub fn stage_kernel(
        &self,
        source: &Path,
        uid: u32,
        gid: u32,
    ) -> Result<(), FirecrackerJailError> {
        let mut source = open_regular_file(source)?;
        if source.metadata()?.len() == 0 {
            return Err(FirecrackerJailError::new("kernel image must not be empty"));
        }
        let name = c_string("vmlinux")?;
        let mut destination = unsafe {
            open_file_descriptor(self.root.as_raw_fd(), &name, CREATE_FILE_FLAGS, 0o600)?
        };
        let stage_result: Result<(), io::Error> = (|| {
            io::copy(&mut source, &mut destination)?;
            destination.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            chown_file(&destination, uid, gid)?;
            destination.sync_all()?;
            self.root.sync_all()?;
            Ok(())
        })();
        if let Err(error) = stage_result {
            let _ = remove_file_at(&self.root, &name);
            let _ = self.root.sync_all();
            return Err(error.into());
        }
        Ok(())
    }

    pub fn stage_rootfs(
        &self,
        path: &Path,
        uid: u32,
        gid: u32,
    ) -> Result<(), FirecrackerJailError> {
        let rootfs = open_regular_file(path)?;
        rootfs.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        chown_file(&rootfs, uid, gid)?;
        rootfs.sync_all()?;
        self.root.sync_all()?;
        Ok(())
    }

    pub fn remove(&mut self) -> Result<(), FirecrackerJailError> {
        if self.removed {
            return Ok(());
        }
        let mut errors = Vec::new();
        if let Err(error) = remove_directory_contents(&self.root, 0) {
            errors.push(error.to_string());
        }
        if let Err(error) = remove_directory_at(&self.root_parent, &self.root_name) {
            errors.push(error.to_string());
        }
        for directory in self.created_parents.iter_mut().rev() {
            if let Err(error) = directory.remove() {
                if error.raw_os_error() == Some(libc::ENOTEMPTY) {
                    break;
                }
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            self.removed = true;
            return Ok(());
        }
        Err(FirecrackerJailError::new(errors.join("; ")))
    }
}

impl Drop for FirecrackerJail {
    fn drop(&mut self) {
        if !self.removed {
            let _ = self.remove();
        }
    }
}

fn path_components(path: &Path) -> Result<Vec<CString>, FirecrackerJailError> {
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(FirecrackerJailError::new(
            "jail root must be an absolute path",
        ));
    }
    if path.as_os_str().as_bytes().contains(&0) {
        return Err(FirecrackerJailError::new("jail root contains a NUL byte"));
    }
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => continue,
            Component::Normal(name) => components.push(c_string(name.as_bytes())?),
            Component::CurDir | Component::ParentDir => {
                return Err(FirecrackerJailError::new(
                    "jail root must not contain . or .. components",
                ));
            }
            Component::Prefix(_) => {
                return Err(FirecrackerJailError::new("path prefixes are not supported"));
            }
        }
    }
    Ok(components)
}

fn c_string(value: impl AsRef<[u8]>) -> Result<CString, FirecrackerJailError> {
    CString::new(value.as_ref()).map_err(|_| FirecrackerJailError::new("path contains a NUL byte"))
}

fn open_root_directory() -> Result<File, FirecrackerJailError> {
    let root = c"/";
    let descriptor = unsafe { libc::open(root.as_ptr(), OPEN_DIRECTORY_FLAGS) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn open_directory_at(parent: &File, name: &CStr) -> io::Result<File> {
    let descriptor =
        unsafe { open_file_descriptor(parent.as_raw_fd(), name, OPEN_DIRECTORY_FLAGS, 0)? };
    Ok(descriptor)
}

fn create_directory_at(parent: &File, name: &CStr) -> io::Result<()> {
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn remove_directory_at(parent: &File, name: &CStr) -> io::Result<()> {
    let result = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::NotFound {
        return Ok(());
    }
    Err(error)
}

fn remove_file_at(parent: &File, name: &CStr) -> io::Result<()> {
    let result = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::NotFound {
        return Ok(());
    }
    Err(error)
}

unsafe fn open_file_descriptor(
    parent: libc::c_int,
    name: &CStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<File> {
    let descriptor = unsafe { libc::openat(parent, name.as_ptr(), flags, mode) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn chown_file(file: &File, uid: u32, gid: u32) -> io::Result<()> {
    let result = unsafe { libc::fchown(file.as_raw_fd(), uid, gid) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn remove_directory_contents(directory: &File, depth: usize) -> io::Result<()> {
    if depth > MAX_DIRECTORY_DEPTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "firecracker jail directory depth exceeded",
        ));
    }
    let entries = directory_entries(directory)?;
    for name in entries {
        if metadata_at(directory, &name)? {
            let child = open_directory_at(directory, &name)?;
            remove_directory_contents(&child, depth + 1)?;
            remove_directory_at(directory, &name)?;
        } else {
            remove_file_at(directory, &name)?;
        }
    }
    Ok(())
}

fn directory_entries(directory: &File) -> io::Result<Vec<CString>> {
    let descriptor = unsafe { libc::dup(directory.as_raw_fd()) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { libc::fdopendir(descriptor) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        unsafe { libc::close(descriptor) };
        return Err(error);
    }
    let stream = DirectoryStream(stream);
    let mut entries = Vec::new();
    loop {
        unsafe {
            *libc::__errno_location() = 0;
        }
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = unsafe { *libc::__errno_location() };
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error));
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        if entries.len() >= MAX_DIRECTORY_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "firecracker jail directory entry count exceeded",
            ));
        }
        entries.push(name.to_owned());
    }
    Ok(entries)
}

struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.0) };
    }
}

fn metadata_at(directory: &File, name: &CStr) -> io::Result<bool> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let metadata = unsafe { metadata.assume_init() };
    Ok((metadata.st_mode & libc::S_IFMT) == libc::S_IFDIR)
}

fn cleanup_created_directories(directories: &mut [CreatedDirectory]) {
    for directory in directories.iter_mut().rev() {
        let _ = directory.remove();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn remove_treats_a_shared_created_ancestor_as_not_empty() {
        let root = tempfile::tempdir().expect("temp directory");
        let shared_parent = root.path().join("jail").join("firecracker");
        let first_root = shared_parent.join("sandbox-1").join("root");
        let second_root = shared_parent.join("sandbox-2").join("root");

        let mut first_jail = FirecrackerJail::create(&first_root).expect("create first jail");
        let mut second_jail = FirecrackerJail::create(&second_root).expect("create second jail");

        first_jail.remove().expect("remove first jail");
        assert!(!first_root.exists());
        assert!(second_root.exists());

        second_jail.remove().expect("remove second jail");
        assert!(!second_root.exists());
        assert!(shared_parent.exists());
    }
}
