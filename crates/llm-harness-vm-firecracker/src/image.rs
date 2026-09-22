//! Ephemeral Firecracker image allocation.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Component, Path, PathBuf};

const OPEN_REGULAR_FILE_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
const OPEN_DIRECTORY_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
const CREATE_DESTINATION_FLAGS: libc::c_int =
    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct FirecrackerImageError {
    message: String,
}

impl FirecrackerImageError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl From<io::Error> for FirecrackerImageError {
    fn from(error: io::Error) -> Self {
        Self::new(format!("firecracker image io error: {error}"))
    }
}

#[derive(Debug)]
pub struct FirecrackerImageAllocator {
    base_image_path: PathBuf,
}

impl FirecrackerImageAllocator {
    pub fn new(base_image_path: impl Into<PathBuf>) -> Self {
        Self {
            base_image_path: base_image_path.into(),
        }
    }

    pub fn allocate(
        &self,
        destination_path: impl AsRef<Path>,
    ) -> Result<FirecrackerImage, FirecrackerImageError> {
        let destination_path = destination_path.as_ref();
        validate_absolute_path("base image", &self.base_image_path)?;
        validate_absolute_path("destination image", destination_path)?;

        let parent_path = destination_path.parent().ok_or_else(|| {
            FirecrackerImageError::new("destination image path must have a parent directory")
        })?;
        let parent = open_directory(parent_path)?;
        let mut source = open_regular_file(&self.base_image_path)?;
        let source_metadata = source.metadata()?;
        if source_metadata.len() == 0 {
            return Err(FirecrackerImageError::new("base image must not be empty"));
        }

        let (mut destination, file_name) = create_destination_file(&parent, destination_path)?;
        let allocation_result = copy_and_sync(&mut source, &mut destination, &parent);
        if let Err(error) = allocation_result {
            let _ = unlink_file(&parent, &file_name);
            let _ = parent.sync_all();
            return Err(error);
        }

        Ok(FirecrackerImage {
            path: destination_path.to_path_buf(),
            parent,
            file_name,
            removed: false,
        })
    }
}

#[derive(Debug)]
pub struct FirecrackerImage {
    path: PathBuf,
    parent: File,
    file_name: CString,
    removed: bool,
}

impl FirecrackerImage {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn remove(&mut self) -> Result<(), FirecrackerImageError> {
        if self.removed {
            return Ok(());
        }
        let remove_result = unlink_file(&self.parent, &self.file_name);
        if remove_result.is_ok() {
            self.removed = true;
            let _ = self.parent.sync_all();
        }
        remove_result.map_err(FirecrackerImageError::from)
    }
}

impl Drop for FirecrackerImage {
    fn drop(&mut self) {
        if !self.removed {
            let _ = unlink_file(&self.parent, &self.file_name);
            let _ = self.parent.sync_all();
        }
    }
}

fn open_regular_file(path: &Path) -> Result<File, FirecrackerImageError> {
    let file = open_path(path, OPEN_REGULAR_FILE_FLAGS)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(FirecrackerImageError::new(
            "base image must be a regular file",
        ));
    }
    Ok(file)
}

fn open_directory(path: &Path) -> Result<File, FirecrackerImageError> {
    let directory = open_path(path, OPEN_DIRECTORY_FLAGS)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() {
        return Err(FirecrackerImageError::new(
            "image parent must be a directory",
        ));
    }
    Ok(directory)
}

fn open_path(path: &Path, final_flags: libc::c_int) -> Result<File, FirecrackerImageError> {
    let component_count = path.components().count();
    let mut current = open_root_directory()?;
    for (component_index, component) in path.components().enumerate() {
        let name = match component {
            Component::RootDir => continue,
            Component::Normal(name) => name,
            Component::CurDir | Component::ParentDir => {
                return Err(FirecrackerImageError::new(
                    "path must not contain . or .. components",
                ));
            }
            Component::Prefix(_) => {
                return Err(FirecrackerImageError::new("path prefix is not supported"));
            }
        };
        let name = CString::new(name.as_bytes())
            .map_err(|_| FirecrackerImageError::new("path contains a NUL byte"))?;
        let flags = if component_index + 1 == component_count {
            final_flags
        } else {
            OPEN_DIRECTORY_FLAGS
        };
        current = openat(&current, &name, flags)?;
    }
    Ok(current)
}

fn open_root_directory() -> Result<File, FirecrackerImageError> {
    let root = CString::new("/").expect("root path is valid");
    openat_descriptor(libc::AT_FDCWD, &root, OPEN_DIRECTORY_FLAGS, 0)
}

fn openat(parent: &File, name: &CStr, flags: libc::c_int) -> Result<File, FirecrackerImageError> {
    openat_descriptor(parent.as_raw_fd(), name, flags, 0)
}

fn openat_descriptor(
    parent: libc::c_int,
    name: &CStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> Result<File, FirecrackerImageError> {
    let file_descriptor = unsafe { libc::openat(parent, name.as_ptr(), flags, mode) };
    if file_descriptor < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(file_descriptor) })
}

fn unlink_file(parent: &File, name: &CStr) -> io::Result<()> {
    let result = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn create_destination_file(
    parent: &File,
    path: &Path,
) -> Result<(File, CString), FirecrackerImageError> {
    let file_name = path.file_name().ok_or_else(|| {
        FirecrackerImageError::new("destination image path must have a file name")
    })?;
    let file_name = CString::new(file_name.as_bytes())
        .map_err(|_| FirecrackerImageError::new("destination image path contains a NUL byte"))?;
    let file = openat_descriptor(
        parent.as_raw_fd(),
        &file_name,
        CREATE_DESTINATION_FLAGS,
        0o600,
    )?;
    Ok((file, file_name))
}

fn copy_and_sync(
    source: &mut File,
    destination: &mut File,
    parent: &File,
) -> Result<(), FirecrackerImageError> {
    io::copy(source, destination)?;
    destination.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    destination.sync_all()?;
    parent.sync_all()?;
    Ok(())
}

fn validate_absolute_path(label: &'static str, path: &Path) -> Result<(), FirecrackerImageError> {
    if path.as_os_str().is_empty() {
        return Err(FirecrackerImageError::new(format!("{label} path is empty")));
    }
    if !path.is_absolute() {
        return Err(FirecrackerImageError::new(format!(
            "{label} path must be absolute"
        )));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        || contains_dot_component(path)
    {
        return Err(FirecrackerImageError::new(format!(
            "{label} path must not contain . or .. components"
        )));
    }
    if path_contains_nul(path) {
        return Err(FirecrackerImageError::new(format!(
            "{label} path contains a NUL byte"
        )));
    }
    Ok(())
}

fn path_contains_nul(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().contains(&0)
}

fn contains_dot_component(path: &Path) -> bool {
    path.as_os_str()
        .as_bytes()
        .split(|byte| *byte == b'/')
        .any(|component| component == b"." || component == b"..")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_base_image(root: &Path) -> PathBuf {
        let path = root.join("base.ext4");
        std::fs::write(&path, b"firecracker-rootfs").expect("write base image");
        path
    }

    #[test]
    fn allocate_copies_base_image_with_private_permissions() {
        let root = tempfile::tempdir().expect("temp directory");
        let base_image = write_base_image(root.path());
        let allocator = FirecrackerImageAllocator::new(&base_image);
        let destination_path = root.path().join("sandbox.ext4");
        let mut image = allocator
            .allocate(&destination_path)
            .expect("allocate image");

        assert_eq!(std::fs::read(image.path()).unwrap(), b"firecracker-rootfs");
        let metadata = std::fs::metadata(image.path()).expect("image metadata");
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        image.remove().expect("remove image");
        assert!(!destination_path.exists());
    }

    #[test]
    fn allocate_rejects_existing_destination() {
        let root = tempfile::tempdir().expect("temp directory");
        let base_image = write_base_image(root.path());
        let destination_path = root.path().join("sandbox.ext4");
        std::fs::write(&destination_path, b"existing").expect("write destination");
        let allocator = FirecrackerImageAllocator::new(&base_image);

        let error = allocator
            .allocate(&destination_path)
            .expect_err("destination already exists");
        assert!(error.to_string().contains("io error"));
        assert_eq!(std::fs::read(&destination_path).unwrap(), b"existing");
    }

    #[test]
    fn drop_removes_unmanaged_image() {
        let root = tempfile::tempdir().expect("temp directory");
        let base_image = write_base_image(root.path());
        let destination_path = root.path().join("sandbox.ext4");
        let allocator = FirecrackerImageAllocator::new(&base_image);
        let image = allocator
            .allocate(&destination_path)
            .expect("allocate image");

        drop(image);
        assert!(!destination_path.exists());
    }

    #[test]
    fn allocate_rejects_base_image_symlink() {
        let root = tempfile::tempdir().expect("temp directory");
        let base_image = write_base_image(root.path());
        let symlink_path = root.path().join("symlink.ext4");
        std::os::unix::fs::symlink(&base_image, &symlink_path).expect("create symlink");
        let allocator = FirecrackerImageAllocator::new(&symlink_path);
        let destination_path = root.path().join("sandbox.ext4");

        let error = allocator
            .allocate(&destination_path)
            .expect_err("base image is a symlink");
        assert!(error.to_string().contains("io error"));
        assert!(!destination_path.exists());
    }

    #[test]
    fn allocate_rejects_empty_base_image() {
        let root = tempfile::tempdir().expect("temp directory");
        let base_image = root.path().join("empty.ext4");
        std::fs::write(&base_image, b"").expect("write empty base image");
        let allocator = FirecrackerImageAllocator::new(&base_image);
        let destination_path = root.path().join("sandbox.ext4");

        let error = allocator
            .allocate(&destination_path)
            .expect_err("base image is empty");
        assert!(error.to_string().contains("base image must not be empty"));
        assert!(!destination_path.exists());
    }

    #[test]
    fn allocate_rejects_destination_parent_symlink() {
        let root = tempfile::tempdir().expect("temp directory");
        let base_image = write_base_image(root.path());
        let actual_parent = tempfile::tempdir().expect("destination parent");
        let symlink_parent = root.path().join("parent");
        std::os::unix::fs::symlink(actual_parent.path(), &symlink_parent)
            .expect("create parent symlink");
        let allocator = FirecrackerImageAllocator::new(&base_image);
        let destination_path = symlink_parent.join("sandbox.ext4");

        let error = allocator
            .allocate(&destination_path)
            .expect_err("destination parent is a symlink");
        assert!(error.to_string().contains("io error"));
        assert!(!actual_parent.path().join("sandbox.ext4").exists());
    }

    #[test]
    fn allocate_rejects_destination_intermediate_parent_symlink() {
        let root = tempfile::tempdir().expect("temp directory");
        let base_image = write_base_image(root.path());
        let actual_parent = tempfile::tempdir().expect("destination parent");
        std::fs::create_dir(actual_parent.path().join("images"))
            .expect("create destination directory");
        let symlink_parent = root.path().join("parent");
        std::os::unix::fs::symlink(actual_parent.path(), &symlink_parent)
            .expect("create intermediate parent symlink");
        let allocator = FirecrackerImageAllocator::new(&base_image);
        let destination_path = symlink_parent.join("images").join("sandbox.ext4");

        let error = allocator
            .allocate(&destination_path)
            .expect_err("destination intermediate parent is a symlink");
        assert!(error.to_string().contains("io error"));
        assert!(
            !actual_parent
                .path()
                .join("images")
                .join("sandbox.ext4")
                .exists()
        );
    }

    #[test]
    fn allocate_rejects_path_traversal_components() {
        let root = tempfile::tempdir().expect("temp directory");
        let base_image = write_base_image(root.path());
        let allocator = FirecrackerImageAllocator::new(&base_image);
        let destination_path = root.path().join("..").join("sandbox.ext4");

        let error = allocator
            .allocate(&destination_path)
            .expect_err("destination contains traversal");
        assert!(
            error
                .to_string()
                .contains("path must not contain . or .. components")
        );
    }

    #[test]
    fn allocate_rejects_current_directory_components() {
        let root = tempfile::tempdir().expect("temp directory");
        let base_image = write_base_image(root.path());
        let allocator = FirecrackerImageAllocator::new(&base_image);
        let mut destination_path = root.path().to_path_buf();
        destination_path.push(".");
        destination_path.push("sandbox.ext4");

        let error = allocator
            .allocate(&destination_path)
            .expect_err("destination contains a current-directory component");
        assert!(
            error
                .to_string()
                .contains("path must not contain . or .. components")
        );
    }
}
