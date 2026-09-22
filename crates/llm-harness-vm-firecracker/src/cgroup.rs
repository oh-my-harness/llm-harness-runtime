//! Cleanup for cgroups created by the Firecracker jailer.

use std::ffi::CString;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::AsRawFd;
use std::path::{Component, Path, PathBuf};

use crate::image::{FirecrackerImageError, open_directory};
use crate::jailer::{
    FirecrackerJailerCgroupVersion, FirecrackerJailerConfig, FirecrackerJailerConfigError,
};

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct FirecrackerCgroupError {
    message: String,
}

impl FirecrackerCgroupError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl From<std::io::Error> for FirecrackerCgroupError {
    fn from(error: std::io::Error) -> Self {
        Self::new(format!("firecracker cgroup io error: {error}"))
    }
}

impl From<FirecrackerJailerConfigError> for FirecrackerCgroupError {
    fn from(error: FirecrackerJailerConfigError) -> Self {
        Self::new(error.to_string())
    }
}

impl From<FirecrackerImageError> for FirecrackerCgroupError {
    fn from(error: FirecrackerImageError) -> Self {
        Self::new(error.to_string())
    }
}

#[derive(Debug, Default)]
pub(crate) struct FirecrackerCgroupCleanup {
    paths: Vec<PathBuf>,
}

impl FirecrackerCgroupCleanup {
    pub(crate) fn plan(
        config: &FirecrackerJailerConfig,
        proc_mounts: &Path,
    ) -> Result<Self, FirecrackerCgroupError> {
        config.validate()?;
        if config.cgroups().is_empty() {
            return Ok(Self::default());
        }

        let version = config
            .cgroup_version()
            .ok_or_else(|| FirecrackerCgroupError::new("cgroup version is missing"))?;
        let parent = config.cgroup_parent()?;
        let mut paths = Vec::new();
        for cgroup in config.cgroups() {
            let controller = cgroup_controller(cgroup)?;
            let hierarchy = hierarchy_path(version, controller, proc_mounts)?;
            let path = hierarchy.join(&parent).join(config.id());
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        Ok(Self { paths })
    }

    pub(crate) fn remove(&self) -> Result<(), FirecrackerCgroupError> {
        let mut errors = Vec::new();
        for path in &self.paths {
            if let Err(error) = remove_cgroup_directory(path) {
                errors.push(format!("{}: {error}", path.display()));
            }
        }
        if errors.is_empty() {
            return Ok(());
        }
        Err(FirecrackerCgroupError::new(errors.join("; ")))
    }
}

impl Drop for FirecrackerCgroupCleanup {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

fn cgroup_controller(cgroup: &str) -> Result<&str, FirecrackerCgroupError> {
    let key = cgroup
        .split_once('=')
        .map(|(key, _)| key)
        .ok_or_else(|| FirecrackerCgroupError::new("cgroup must be in key=value format"))?;
    key.split_once('.')
        .map(|(controller, _)| controller)
        .ok_or_else(|| {
            FirecrackerCgroupError::new("cgroup key must be in controller.property format")
        })
}

fn hierarchy_path(
    version: FirecrackerJailerCgroupVersion,
    controller: &str,
    proc_mounts: &Path,
) -> Result<PathBuf, FirecrackerCgroupError> {
    let file = File::open(proc_mounts)?;
    for line in BufReader::new(file).lines() {
        let line = line?;
        let Some((mount_point, filesystem_type, options)) = parse_mount_line(&line) else {
            continue;
        };
        let matches = match version {
            FirecrackerJailerCgroupVersion::V1 => {
                filesystem_type == "cgroup"
                    && options
                        .split(',')
                        .any(|mounted_controller| mounted_controller == controller)
            }
            FirecrackerJailerCgroupVersion::V2 => filesystem_type == "cgroup2",
        };
        if matches {
            let hierarchy = validate_mount_point(mount_point)?;
            if version == FirecrackerJailerCgroupVersion::V2 {
                validate_v2_controller(&hierarchy, controller)?;
            }
            return Ok(hierarchy);
        }
    }
    Err(FirecrackerCgroupError::new(format!(
        "cgroup hierarchy for controller {controller} is missing"
    )))
}

fn parse_mount_line(line: &str) -> Option<(&str, &str, &str)> {
    let mut fields = line.split_whitespace();
    let _source = fields.next()?;
    let mount_point = fields.next()?;
    let filesystem_type = fields.next()?;
    let options = fields.next()?;
    let _dump_frequency = fields.next()?;
    let _pass_number = fields.next()?;
    Some((mount_point, filesystem_type, options))
}

fn validate_mount_point(mount_point: &str) -> Result<PathBuf, FirecrackerCgroupError> {
    if mount_point.is_empty() || mount_point.contains('\0') {
        return Err(FirecrackerCgroupError::new("cgroup mount point is invalid"));
    }
    let path = PathBuf::from(mount_point);
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(FirecrackerCgroupError::new(
            "cgroup mount point must be an absolute path without . or .. components",
        ));
    }
    Ok(path)
}

fn validate_v2_controller(
    hierarchy: &Path,
    controller: &str,
) -> Result<(), FirecrackerCgroupError> {
    let file = File::open(hierarchy.join("cgroup.controllers"))?;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.split_whitespace().any(|value| value == controller) {
            return Ok(());
        }
    }
    Err(FirecrackerCgroupError::new(format!(
        "cgroup v2 controller {controller} is unavailable"
    )))
}

fn remove_cgroup_directory(path: &Path) -> Result<(), FirecrackerCgroupError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(FirecrackerCgroupError::new(
                "firecracker cgroup path is not a directory",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }

    let parent_path = path.parent().ok_or_else(|| {
        FirecrackerCgroupError::new("firecracker cgroup path must have a parent directory")
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        FirecrackerCgroupError::new("firecracker cgroup path must have a file name")
    })?;
    let file_name = CString::new(file_name.as_bytes())
        .map_err(|_| FirecrackerCgroupError::new("cgroup path contains a NUL byte"))?;
    let parent = open_directory(parent_path)?;
    let result =
        unsafe { libc::unlinkat(parent.as_raw_fd(), file_name.as_ptr(), libc::AT_REMOVEDIR) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::NotFound {
        return Ok(());
    }
    Err(error.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_proc_mounts(path: &Path, contents: &str) {
        fs::write(path, contents).expect("write proc mounts");
    }

    fn jailer_config() -> FirecrackerJailerConfig {
        FirecrackerJailerConfig::new(
            "/usr/local/bin/firecracker-jailer",
            "/usr/local/bin/firecracker",
            "sandbox-1",
            1000,
            1000,
        )
    }

    #[test]
    fn plan_derives_one_v2_cgroup_path() {
        let root = tempfile::tempdir().expect("temp directory");
        let unified = root.path().join("unified");
        let proc_mounts = root.path().join("proc-mounts");
        fs::create_dir(&unified).expect("create unified cgroup hierarchy");
        fs::write(unified.join("cgroup.controllers"), "cpuset cpu io memory\n")
            .expect("write available v2 controllers");
        write_proc_mounts(
            &proc_mounts,
            &format!(
                "cgroup2 {} cgroup2 rw,nosuid,nodev,noexec 0 0\n",
                unified.display()
            ),
        );
        let config = jailer_config()
            .with_cgroup_version(FirecrackerJailerCgroupVersion::V2)
            .with_cgroups(vec![
                "cpu.max=100000".to_owned(),
                "memory.max=1073741824".to_owned(),
            ]);

        let cleanup =
            FirecrackerCgroupCleanup::plan(&config, &proc_mounts).expect("plan v2 cgroup cleanup");
        assert_eq!(cleanup.paths, vec![unified.join("firecracker/sandbox-1")]);
    }

    #[test]
    fn plan_derives_v1_controller_paths_and_removes_only_jail_ids() {
        let root = tempfile::tempdir().expect("temp directory");
        let cpuset = root.path().join("cpuset");
        let cpu = root.path().join("cpu");
        let proc_mounts = root.path().join("proc-mounts");
        fs::create_dir(&cpuset).expect("create cpuset cgroup hierarchy");
        fs::create_dir(&cpu).expect("create cpu cgroup hierarchy");
        fs::create_dir(cpuset.join("parent")).expect("create cpuset parent");
        fs::create_dir(cpu.join("parent")).expect("create cpu parent");
        fs::create_dir(cpuset.join("parent/sandbox-1")).expect("create cpuset jail id");
        fs::create_dir(cpu.join("parent/sandbox-1")).expect("create cpu jail id");
        write_proc_mounts(
            &proc_mounts,
            &format!(
                "cgroup {} cgroup rw,nosuid,nodev,noexec,cpuset 0 0\n\
                 cgroup {} cgroup rw,nosuid,nodev,noexec,cpu 0 0\n",
                cpuset.display(),
                cpu.display()
            ),
        );
        let config = jailer_config()
            .with_cgroup_version(FirecrackerJailerCgroupVersion::V1)
            .with_parent_cgroup("parent")
            .with_cgroups(vec![
                "cpuset.cpus=0".to_owned(),
                "cpu.weight=100".to_owned(),
            ]);

        let cleanup =
            FirecrackerCgroupCleanup::plan(&config, &proc_mounts).expect("plan v1 cgroup cleanup");
        assert_eq!(
            cleanup.paths,
            vec![
                cpuset.join("parent/sandbox-1"),
                cpu.join("parent/sandbox-1")
            ]
        );
        cleanup.remove().expect("remove v1 cgroups");
        cleanup.remove().expect("remove v1 cgroups again");
        assert!(!cpuset.join("parent/sandbox-1").exists());
        assert!(!cpu.join("parent/sandbox-1").exists());
        assert!(cpuset.join("parent").exists());
        assert!(cpu.join("parent").exists());
    }

    #[test]
    fn plan_rejects_a_missing_controller_hierarchy() {
        let root = tempfile::tempdir().expect("temp directory");
        let proc_mounts = root.path().join("proc-mounts");
        write_proc_mounts(&proc_mounts, "none /not-a-cgroup tmpfs rw 0 0\n");
        let config = jailer_config()
            .with_cgroup_version(FirecrackerJailerCgroupVersion::V1)
            .with_cgroups(vec!["cpuset.cpus=0".to_owned()]);

        let error = FirecrackerCgroupCleanup::plan(&config, &proc_mounts)
            .expect_err("missing cgroup hierarchy");
        assert!(error.to_string().contains("controller cpuset"));
    }

    #[test]
    fn plan_rejects_an_unavailable_v2_controller() {
        let root = tempfile::tempdir().expect("temp directory");
        let unified = root.path().join("unified");
        let proc_mounts = root.path().join("proc-mounts");
        fs::create_dir(&unified).expect("create unified cgroup hierarchy");
        fs::write(unified.join("cgroup.controllers"), "cpu memory\n")
            .expect("write available v2 controllers");
        write_proc_mounts(
            &proc_mounts,
            &format!(
                "cgroup2 {} cgroup2 rw,nosuid,nodev,noexec 0 0\n",
                unified.display()
            ),
        );
        let config = jailer_config()
            .with_cgroup_version(FirecrackerJailerCgroupVersion::V2)
            .with_cgroups(vec!["missing.max=1".to_owned()]);

        let error = FirecrackerCgroupCleanup::plan(&config, &proc_mounts)
            .expect_err("unavailable v2 controller");
        assert!(error.to_string().contains("controller missing"));
    }

    #[test]
    fn plan_returns_no_paths_without_cgroup_settings() {
        let root = tempfile::tempdir().expect("temp directory");
        let proc_mounts = root.path().join("proc-mounts");
        write_proc_mounts(&proc_mounts, "");

        let cleanup = FirecrackerCgroupCleanup::plan(&jailer_config(), &proc_mounts)
            .expect("plan empty cgroup cleanup");
        cleanup.remove().expect("remove empty cgroup cleanup");
        assert!(cleanup.paths.is_empty());
    }
}
