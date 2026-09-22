use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::process::FirecrackerProcessConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirecrackerJailerCgroupVersion {
    V1,
    V2,
}

#[derive(Debug, Clone)]
pub struct FirecrackerJailerConfig {
    jailer_binary_path: PathBuf,
    firecracker_binary_path: PathBuf,
    id: String,
    uid: u32,
    gid: u32,
    chroot_base_dir: PathBuf,
    cgroup_version: Option<FirecrackerJailerCgroupVersion>,
    cgroups: Vec<String>,
    parent_cgroup: Option<String>,
    netns: Option<PathBuf>,
    resource_limits: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct FirecrackerJailerConfigError {
    message: String,
}

impl FirecrackerJailerConfigError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl FirecrackerJailerConfig {
    pub fn new(
        jailer_binary_path: impl Into<PathBuf>,
        firecracker_binary_path: impl Into<PathBuf>,
        id: impl Into<String>,
        uid: u32,
        gid: u32,
    ) -> Self {
        Self {
            jailer_binary_path: jailer_binary_path.into(),
            firecracker_binary_path: firecracker_binary_path.into(),
            id: id.into(),
            uid,
            gid,
            chroot_base_dir: PathBuf::from("/srv/jailer"),
            cgroup_version: None,
            cgroups: Vec::new(),
            parent_cgroup: None,
            netns: None,
            resource_limits: Vec::new(),
        }
    }

    pub fn with_chroot_base_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.chroot_base_dir = path.into();
        self
    }

    pub fn with_cgroup_version(mut self, version: FirecrackerJailerCgroupVersion) -> Self {
        self.cgroup_version = Some(version);
        self
    }

    pub fn with_cgroups(mut self, cgroups: Vec<String>) -> Self {
        self.cgroups = cgroups;
        self
    }

    pub fn with_parent_cgroup(mut self, parent_cgroup: impl Into<String>) -> Self {
        self.parent_cgroup = Some(parent_cgroup.into());
        self
    }

    pub fn with_netns(mut self, path: impl Into<PathBuf>) -> Self {
        self.netns = Some(path.into());
        self
    }

    pub fn with_resource_limits(mut self, resource_limits: Vec<String>) -> Self {
        self.resource_limits = resource_limits;
        self
    }

    pub fn validate(&self) -> Result<(), FirecrackerJailerConfigError> {
        validate_host_path("jailer binary", &self.jailer_binary_path)?;
        validate_host_path("Firecracker binary", &self.firecracker_binary_path)?;
        if self.jailer_binary_path.file_name().is_none()
            || self.firecracker_binary_path.file_name().is_none()
        {
            return Err(FirecrackerJailerConfigError::invalid(
                "jailer and Firecracker binary paths must include a file name",
            ));
        }
        if self.jailer_binary_path == self.firecracker_binary_path {
            return Err(FirecrackerJailerConfigError::invalid(
                "jailer binary and Firecracker binary paths must be different",
            ));
        }
        validate_id(&self.id)?;
        if self.uid == 0 || self.gid == 0 {
            return Err(FirecrackerJailerConfigError::invalid(
                "jailer uid and gid must be non-zero",
            ));
        }
        validate_host_path("chroot base directory", &self.chroot_base_dir)?;
        if self.chroot_base_dir == Path::new("/") {
            return Err(FirecrackerJailerConfigError::invalid(
                "chroot base directory must not be the filesystem root",
            ));
        }
        if !self.cgroups.is_empty() && self.cgroup_version.is_none() {
            return Err(FirecrackerJailerConfigError::invalid(
                "cgroup version must be explicit when cgroup settings are set",
            ));
        }
        validate_key_values("cgroup setting", &self.cgroups)?;
        validate_resource_limits(&self.resource_limits)?;
        if let Some(parent_cgroup) = &self.parent_cgroup {
            validate_parent_cgroup(parent_cgroup)?;
        }
        if let Some(netns) = &self.netns {
            validate_host_path("netns", netns)?;
        }
        Ok(())
    }

    pub fn chroot_dir(&self) -> Result<PathBuf, FirecrackerJailerConfigError> {
        self.validate()?;
        let firecracker_file_name = self.firecracker_binary_path.file_name().ok_or_else(|| {
            FirecrackerJailerConfigError::invalid(
                "Firecracker binary path must include a file name",
            )
        })?;
        Ok(self
            .chroot_base_dir
            .join(firecracker_file_name)
            .join(&self.id)
            .join("root"))
    }

    pub fn command_args(
        &self,
        process_config: &FirecrackerProcessConfig,
    ) -> Result<Vec<OsString>, FirecrackerJailerConfigError> {
        self.validate()?;
        let chroot_dir = self.chroot_dir()?;
        process_config
            .validate()
            .map_err(|error| FirecrackerJailerConfigError::invalid(error.to_string()))?;
        if process_config.binary_path() != self.firecracker_binary_path {
            return Err(FirecrackerJailerConfigError::invalid(
                "process binary path must match the jailer Firecracker binary path",
            ));
        }
        let api_socket_path =
            jail_path(&chroot_dir, process_config.api_socket_path(), "API socket")?;
        let config_file_path = jail_path(
            &chroot_dir,
            process_config.config_file_path(),
            "config file",
        )?;

        let mut args = vec![
            OsString::from("--id"),
            OsString::from(self.id.clone()),
            OsString::from("--exec-file"),
            self.firecracker_binary_path.as_os_str().to_owned(),
            OsString::from("--uid"),
            OsString::from(self.uid.to_string()),
            OsString::from("--gid"),
            OsString::from(self.gid.to_string()),
            OsString::from("--chroot-base-dir"),
            self.chroot_base_dir.as_os_str().to_owned(),
        ];
        if let Some(version) = self.cgroup_version {
            args.push(OsString::from("--cgroup-version"));
            args.push(OsString::from(match version {
                FirecrackerJailerCgroupVersion::V1 => "1",
                FirecrackerJailerCgroupVersion::V2 => "2",
            }));
        }
        for cgroup in &self.cgroups {
            args.push(OsString::from("--cgroup"));
            args.push(OsString::from(cgroup.clone()));
        }
        if let Some(parent_cgroup) = &self.parent_cgroup {
            args.push(OsString::from("--parent-cgroup"));
            args.push(OsString::from(parent_cgroup.clone()));
        }
        if let Some(netns) = &self.netns {
            args.push(OsString::from("--netns"));
            args.push(netns.as_os_str().to_owned());
        }
        for resource_limit in &self.resource_limits {
            args.push(OsString::from("--resource-limit"));
            args.push(OsString::from(resource_limit.clone()));
        }
        args.push(OsString::from("--"));
        args.push(OsString::from("--api-sock"));
        args.push(api_socket_path.as_os_str().to_owned());
        args.push(OsString::from("--config-file"));
        args.push(config_file_path.as_os_str().to_owned());
        Ok(args)
    }
}

fn validate_host_path(
    label: &'static str,
    path: &Path,
) -> Result<(), FirecrackerJailerConfigError> {
    if path.as_os_str().is_empty() {
        return Err(FirecrackerJailerConfigError::invalid(format!(
            "{label} path is empty"
        )));
    }
    if !path.is_absolute() {
        return Err(FirecrackerJailerConfigError::invalid(format!(
            "{label} path must be absolute"
        )));
    }
    if path.to_str().is_none() {
        return Err(FirecrackerJailerConfigError::invalid(format!(
            "{label} path must be valid UTF-8"
        )));
    }
    if path_contains_nul(path) {
        return Err(FirecrackerJailerConfigError::invalid(format!(
            "{label} path contains a NUL byte"
        )));
    }
    if contains_dot_component(path) {
        return Err(FirecrackerJailerConfigError::invalid(format!(
            "{label} path must not contain . or .. components"
        )));
    }
    Ok(())
}

fn jail_path(
    chroot_dir: &Path,
    host_path: &Path,
    label: &'static str,
) -> Result<PathBuf, FirecrackerJailerConfigError> {
    if !host_path.starts_with(chroot_dir) {
        return Err(FirecrackerJailerConfigError::invalid(format!(
            "{label} host path must be inside the jailer chroot"
        )));
    }
    let relative_path = host_path.strip_prefix(chroot_dir).map_err(|_| {
        FirecrackerJailerConfigError::invalid(format!(
            "{label} host path must be inside the jailer chroot"
        ))
    })?;
    if relative_path.as_os_str().is_empty() {
        return Err(FirecrackerJailerConfigError::invalid(format!(
            "{label} host path must not be the jailer chroot"
        )));
    }
    Ok(Path::new("/").join(relative_path))
}

fn validate_id(id: &str) -> Result<(), FirecrackerJailerConfigError> {
    if id.is_empty() {
        return Err(FirecrackerJailerConfigError::invalid("jailer id is empty"));
    }
    if id.len() > 64 {
        return Err(FirecrackerJailerConfigError::invalid(
            "jailer id must be at most 64 bytes",
        ));
    }
    if id.contains('\0') {
        return Err(FirecrackerJailerConfigError::invalid(
            "jailer id contains a NUL byte",
        ));
    }
    if id.contains('/') || id == "." || id == ".." {
        return Err(FirecrackerJailerConfigError::invalid(
            "jailer id must be a single path component",
        ));
    }
    if !id
        .chars()
        .all(|character| character == '-' || character.is_alphanumeric())
    {
        return Err(FirecrackerJailerConfigError::invalid(
            "jailer id must contain only alphanumeric characters or hyphens",
        ));
    }
    Ok(())
}

fn validate_key_values(
    label: &'static str,
    values: &[String],
) -> Result<(), FirecrackerJailerConfigError> {
    let mut keys = HashSet::new();
    for value in values {
        let Some((key, value)) = value.split_once('=') else {
            return Err(FirecrackerJailerConfigError::invalid(format!(
                "{label} must be in key=value format"
            )));
        };
        if key.is_empty() || key.contains(char::is_whitespace) {
            return Err(FirecrackerJailerConfigError::invalid(format!(
                "{label} key is invalid"
            )));
        }
        if key.contains('\0')
            || Path::new(key).is_absolute()
            || contains_dot_component(Path::new(key))
        {
            return Err(FirecrackerJailerConfigError::invalid(format!(
                "{label} key is invalid"
            )));
        }
        if value.trim().is_empty() || value.contains('\0') {
            return Err(FirecrackerJailerConfigError::invalid(format!(
                "{label} value is invalid"
            )));
        }
        if !keys.insert(key.to_owned()) {
            return Err(FirecrackerJailerConfigError::invalid(format!(
                "{label} key is duplicate: {key}"
            )));
        }
    }
    Ok(())
}

fn validate_resource_limits(values: &[String]) -> Result<(), FirecrackerJailerConfigError> {
    let mut keys = HashSet::new();
    for value in values {
        let Some((key, limit)) = value.split_once('=') else {
            return Err(FirecrackerJailerConfigError::invalid(
                "resource limit must be in key=value format",
            ));
        };
        if key != "fsize" && key != "no-file" {
            return Err(FirecrackerJailerConfigError::invalid(
                "resource limit key must be fsize or no-file",
            ));
        }
        if limit.parse::<u64>().is_err() {
            return Err(FirecrackerJailerConfigError::invalid(
                "resource limit value must be a non-negative integer",
            ));
        }
        if !keys.insert(key.to_owned()) {
            return Err(FirecrackerJailerConfigError::invalid(format!(
                "resource limit key is duplicate: {key}"
            )));
        }
    }
    Ok(())
}

fn validate_parent_cgroup(parent_cgroup: &str) -> Result<(), FirecrackerJailerConfigError> {
    if parent_cgroup.is_empty() {
        return Err(FirecrackerJailerConfigError::invalid(
            "parent cgroup is empty",
        ));
    }
    if parent_cgroup.contains('\0') {
        return Err(FirecrackerJailerConfigError::invalid(
            "parent cgroup contains a NUL byte",
        ));
    }
    let path = Path::new(parent_cgroup);
    if path.is_absolute() || contains_dot_component(path) {
        return Err(FirecrackerJailerConfigError::invalid(
            "parent cgroup must be a relative path without . or .. components",
        ));
    }
    Ok(())
}

fn path_contains_nul(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().contains(&0)
}

fn contains_dot_component(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str()
        .as_bytes()
        .split(|byte| *byte == b'/')
        .any(|component| component == b"." || component == b"..")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_jailer_config() -> FirecrackerJailerConfig {
        FirecrackerJailerConfig::new(
            "/usr/local/bin/jailer",
            "/usr/local/bin/firecracker",
            "sandbox-1",
            1000,
            1000,
        )
    }

    fn valid_process_config() -> FirecrackerProcessConfig {
        FirecrackerProcessConfig::new(
            "/usr/local/bin/firecracker",
            "/srv/jailer/firecracker/sandbox-1/root/run/firecracker.sock",
            "/srv/jailer/firecracker/sandbox-1/root/run/firecracker.json",
        )
    }

    #[test]
    fn config_builds_default_jailer_command_args() {
        let config = valid_jailer_config();
        let args = config
            .command_args(&valid_process_config())
            .expect("valid jailer command");
        assert_eq!(
            args,
            vec![
                OsString::from("--id"),
                OsString::from("sandbox-1"),
                OsString::from("--exec-file"),
                OsString::from("/usr/local/bin/firecracker"),
                OsString::from("--uid"),
                OsString::from("1000"),
                OsString::from("--gid"),
                OsString::from("1000"),
                OsString::from("--chroot-base-dir"),
                OsString::from("/srv/jailer"),
                OsString::from("--"),
                OsString::from("--api-sock"),
                OsString::from("/run/firecracker.sock"),
                OsString::from("--config-file"),
                OsString::from("/run/firecracker.json"),
            ]
        );
    }

    #[test]
    fn config_builds_optional_jailer_command_args() {
        let config = valid_jailer_config()
            .with_chroot_base_dir("/var/lib/firecracker")
            .with_cgroup_version(FirecrackerJailerCgroupVersion::V2)
            .with_cgroups(vec![
                "cpuset.cpus=0".to_owned(),
                "memory.max=268435456".to_owned(),
            ])
            .with_parent_cgroup("llm-harness")
            .with_netns("/run/netns/sandbox-1")
            .with_resource_limits(vec!["no-file=1024".to_owned()]);
        let process_config = FirecrackerProcessConfig::new(
            "/usr/local/bin/firecracker",
            "/var/lib/firecracker/firecracker/sandbox-1/root/run/firecracker.sock",
            "/var/lib/firecracker/firecracker/sandbox-1/root/run/firecracker.json",
        );
        let args = config
            .command_args(&process_config)
            .expect("valid jailer command");
        assert_eq!(
            args,
            vec![
                OsString::from("--id"),
                OsString::from("sandbox-1"),
                OsString::from("--exec-file"),
                OsString::from("/usr/local/bin/firecracker"),
                OsString::from("--uid"),
                OsString::from("1000"),
                OsString::from("--gid"),
                OsString::from("1000"),
                OsString::from("--chroot-base-dir"),
                OsString::from("/var/lib/firecracker"),
                OsString::from("--cgroup-version"),
                OsString::from("2"),
                OsString::from("--cgroup"),
                OsString::from("cpuset.cpus=0"),
                OsString::from("--cgroup"),
                OsString::from("memory.max=268435456"),
                OsString::from("--parent-cgroup"),
                OsString::from("llm-harness"),
                OsString::from("--netns"),
                OsString::from("/run/netns/sandbox-1"),
                OsString::from("--resource-limit"),
                OsString::from("no-file=1024"),
                OsString::from("--"),
                OsString::from("--api-sock"),
                OsString::from("/run/firecracker.sock"),
                OsString::from("--config-file"),
                OsString::from("/run/firecracker.json"),
            ]
        );
    }

    #[test]
    fn config_derives_the_jailer_chroot_directory() {
        let config = valid_jailer_config();
        assert_eq!(
            config.chroot_dir().expect("valid chroot directory"),
            PathBuf::from("/srv/jailer/firecracker/sandbox-1/root")
        );
    }

    #[test]
    fn command_args_rejects_artifacts_outside_the_jailer_chroot() {
        let process_config = FirecrackerProcessConfig::new(
            "/usr/local/bin/firecracker",
            "/run/firecracker/sandbox-1.sock",
            "/srv/jailer/firecracker/sandbox-1/root/run/firecracker.json",
        );
        let error = valid_jailer_config()
            .command_args(&process_config)
            .expect_err("API socket outside chroot");
        assert!(
            error
                .to_string()
                .contains("API socket host path must be inside the jailer chroot"),
            "{error}"
        );
    }

    #[test]
    fn command_args_rejects_a_mismatched_process_binary() {
        let process_config = FirecrackerProcessConfig::new(
            "/usr/bin/firecracker",
            "/srv/jailer/firecracker/sandbox-1/root/run/firecracker.sock",
            "/srv/jailer/firecracker/sandbox-1/root/run/firecracker.json",
        );
        let error = valid_jailer_config()
            .command_args(&process_config)
            .expect_err("mismatched process binary");
        assert!(
            error
                .to_string()
                .contains("process binary path must match the jailer Firecracker binary path"),
            "{error}"
        );
    }

    #[test]
    fn config_rejects_invalid_paths_and_ids() {
        let cases = [
            (
                FirecrackerJailerConfig::new(
                    "jailer",
                    "/usr/local/bin/firecracker",
                    "sandbox-1",
                    1000,
                    1000,
                ),
                "jailer binary path must be absolute",
            ),
            (
                FirecrackerJailerConfig::new(
                    "/usr/local/bin/jailer",
                    "/usr/local/bin/../firecracker",
                    "sandbox-1",
                    1000,
                    1000,
                ),
                "Firecracker binary path must not contain . or .. components",
            ),
            (
                FirecrackerJailerConfig::new("/usr/local/bin/jailer", "/", "sandbox-1", 1000, 1000),
                "jailer and Firecracker binary paths must include a file name",
            ),
            (
                FirecrackerJailerConfig::new(
                    "/usr/local/bin/jailer",
                    "/usr/local/bin/firecracker",
                    "nested/sandbox",
                    1000,
                    1000,
                ),
                "jailer id must be a single path component",
            ),
            (
                FirecrackerJailerConfig::new(
                    "/usr/local/bin/jailer",
                    "/usr/local/bin/firecracker",
                    "sandbox_1",
                    1000,
                    1000,
                ),
                "jailer id must contain only alphanumeric characters or hyphens",
            ),
            (
                FirecrackerJailerConfig::new(
                    "/usr/local/bin/jailer",
                    "/usr/local/bin/firecracker",
                    "a".repeat(65),
                    1000,
                    1000,
                ),
                "jailer id must be at most 64 bytes",
            ),
            (
                FirecrackerJailerConfig::new(
                    "/usr/local/bin/jailer",
                    "/usr/local/bin/firecracker",
                    "sandbox-1",
                    0,
                    1000,
                ),
                "jailer uid and gid must be non-zero",
            ),
        ];
        for (config, expected) in cases {
            let error = config.validate().expect_err("invalid jailer config");
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn config_rejects_non_utf8_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let firecracker_binary_path = PathBuf::from(OsString::from_vec(
            b"/usr/local/bin/firecracker\xff".to_vec(),
        ));
        let config = FirecrackerJailerConfig::new(
            "/usr/local/bin/jailer",
            firecracker_binary_path,
            "sandbox-1",
            1000,
            1000,
        );
        let error = config.validate().expect_err("non-UTF-8 Firecracker binary");
        assert!(
            error
                .to_string()
                .contains("Firecracker binary path must be valid UTF-8"),
            "{error}"
        );
    }

    #[test]
    fn config_rejects_the_filesystem_root_as_chroot_base() {
        let config = valid_jailer_config().with_chroot_base_dir("/");
        let error = config
            .validate()
            .expect_err("filesystem root as chroot base");
        assert!(
            error
                .to_string()
                .contains("chroot base directory must not be the filesystem root"),
            "{error}"
        );
    }

    #[test]
    fn config_rejects_same_binaries_and_nul_paths() {
        let config = FirecrackerJailerConfig::new(
            "/usr/local/bin/jailer\0",
            "/usr/local/bin/jailer\0",
            "sandbox-1",
            1000,
            1000,
        );
        let error = config.validate().expect_err("invalid jailer config");
        assert!(error.to_string().contains("NUL byte"), "{error}");
    }

    #[test]
    fn config_requires_explicit_cgroup_version() {
        let config = valid_jailer_config().with_cgroups(vec!["cpuset.cpus=0".to_owned()]);
        let error = config.validate().expect_err("cgroup version is implicit");
        assert!(
            error
                .to_string()
                .contains("cgroup version must be explicit"),
            "{error}"
        );
    }

    #[test]
    fn config_rejects_invalid_and_duplicate_key_values() {
        let cgroup_cases = [
            vec!["cpuset.cpus".to_owned()],
            vec![" cpu.max=100".to_owned()],
            vec!["cpu.max=".to_owned()],
            vec!["../cpu.max=100".to_owned()],
            vec!["cpu.max=1".to_owned(), "cpu.max=2".to_owned()],
        ];
        for cgroups in cgroup_cases {
            let config = valid_jailer_config()
                .with_cgroup_version(FirecrackerJailerCgroupVersion::V2)
                .with_cgroups(cgroups);
            let error = config.validate().expect_err("invalid cgroup setting");
            assert!(error.to_string().contains("cgroup setting"), "{error}");
        }

        let resource_limit_cases = [
            vec!["no-file".to_owned()],
            vec!["file-size=1024".to_owned()],
            vec!["no-file=many".to_owned()],
            vec!["no-file=1024".to_owned(), "no-file=2048".to_owned()],
        ];
        for resource_limits in resource_limit_cases {
            let config = valid_jailer_config().with_resource_limits(resource_limits);
            let error = config.validate().expect_err("invalid resource limit");
            assert!(error.to_string().contains("resource limit"), "{error}");
        }
    }

    #[test]
    fn config_rejects_invalid_parent_cgroup_and_netns() {
        let config = valid_jailer_config().with_parent_cgroup("/absolute");
        let error = config.validate().expect_err("absolute parent cgroup");
        assert!(
            error
                .to_string()
                .contains("parent cgroup must be a relative path without . or .. components"),
            "{error}"
        );

        let config = valid_jailer_config().with_netns("relative/netns");
        let error = config.validate().expect_err("relative netns");
        assert!(
            error.to_string().contains("netns path must be absolute"),
            "{error}"
        );
    }

    #[test]
    fn command_args_validates_process_config() {
        let process_config = FirecrackerProcessConfig::new(
            "/usr/local/bin/firecracker",
            "sandbox.sock",
            "/run/firecracker/sandbox.json",
        );
        let error = valid_jailer_config()
            .command_args(&process_config)
            .expect_err("invalid process config");
        assert!(
            error
                .to_string()
                .contains("API socket path must be absolute"),
            "{error}"
        );
    }
}
