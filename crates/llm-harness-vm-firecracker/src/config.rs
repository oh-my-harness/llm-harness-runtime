use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct FirecrackerConfigError {
    message: String,
}

impl FirecrackerConfigError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FirecrackerConfig {
    kernel_image_path: PathBuf,
    boot_args: String,
    rootfs_path: PathBuf,
    rootfs_read_only: bool,
    vcpu_count: u32,
    mem_size_mib: u32,
    vsock_guest_cid: u32,
    vsock_uds_path: PathBuf,
}

impl FirecrackerConfig {
    pub fn new(
        kernel_image_path: impl Into<PathBuf>,
        rootfs_path: impl Into<PathBuf>,
        vsock_uds_path: impl Into<PathBuf>,
        vcpu_count: u32,
        mem_size_mib: u32,
        vsock_guest_cid: u32,
    ) -> Self {
        Self {
            kernel_image_path: kernel_image_path.into(),
            boot_args: String::new(),
            rootfs_path: rootfs_path.into(),
            rootfs_read_only: false,
            vcpu_count,
            mem_size_mib,
            vsock_guest_cid,
            vsock_uds_path: vsock_uds_path.into(),
        }
    }

    pub fn with_boot_args(mut self, boot_args: impl Into<String>) -> Self {
        self.boot_args = boot_args.into();
        self
    }

    pub fn with_rootfs_read_only(mut self, read_only: bool) -> Self {
        self.rootfs_read_only = read_only;
        self
    }

    pub fn vsock_uds_path(&self) -> &Path {
        &self.vsock_uds_path
    }

    pub fn validate(&self) -> Result<(), FirecrackerConfigError> {
        validate_absolute_path("kernel image", &self.kernel_image_path)?;
        validate_absolute_path("rootfs", &self.rootfs_path)?;
        validate_absolute_path("vsock UDS", &self.vsock_uds_path)?;
        if self.vcpu_count == 0 {
            return Err(FirecrackerConfigError::invalid(
                "vcpu count must be positive",
            ));
        }
        if self.mem_size_mib == 0 {
            return Err(FirecrackerConfigError::invalid(
                "memory size must be positive",
            ));
        }
        if self.vsock_guest_cid < 3 {
            return Err(FirecrackerConfigError::invalid(
                "vsock guest CID must be at least 3",
            ));
        }
        Ok(())
    }

    pub fn to_json(&self) -> Result<serde_json::Value, FirecrackerConfigError> {
        self.validate()?;
        let kernel_image_path = self.kernel_image_path.to_string_lossy().to_string();
        let rootfs_path = self.rootfs_path.to_string_lossy().to_string();
        let vsock_uds_path = self.vsock_uds_path.to_string_lossy().to_string();
        Ok(self.config_json(kernel_image_path, rootfs_path, vsock_uds_path))
    }

    pub fn to_jailed_json(
        &self,
        jail_root: &Path,
    ) -> Result<serde_json::Value, FirecrackerConfigError> {
        self.validate()?;
        let kernel_image_path = jailed_path("kernel image", jail_root, &self.kernel_image_path)?;
        let rootfs_path = jailed_path("rootfs", jail_root, &self.rootfs_path)?;
        let vsock_uds_path = jailed_path("vsock UDS", jail_root, &self.vsock_uds_path)?;
        Ok(self.config_json(kernel_image_path, rootfs_path, vsock_uds_path))
    }

    fn config_json(
        &self,
        kernel_image_path: String,
        rootfs_path: String,
        vsock_uds_path: String,
    ) -> serde_json::Value {
        serde_json::json!({
            "boot-source": {
                "kernel_image_path": kernel_image_path,
                "boot_args": self.boot_args,
            },
            "drives": [{
                "drive_id": "rootfs",
                "path_on_host": rootfs_path,
                "is_root_device": true,
                "is_read_only": self.rootfs_read_only,
            }],
            "machine-config": {
                "vcpu_count": self.vcpu_count,
                "mem_size_mib": self.mem_size_mib,
                "smt": false,
            },
            "network-interfaces": [],
            "vsock": {
                "guest_cid": self.vsock_guest_cid,
                "uds_path": vsock_uds_path,
            },
        })
    }
}

fn jailed_path(
    label: &'static str,
    jail_root: &Path,
    host_path: &Path,
) -> Result<String, FirecrackerConfigError> {
    if !host_path.starts_with(jail_root) {
        return Err(FirecrackerConfigError::invalid(format!(
            "{label} host path must be inside the jail root"
        )));
    }
    let relative_path = host_path.strip_prefix(jail_root).map_err(|_| {
        FirecrackerConfigError::invalid(format!("{label} host path must be inside the jail root"))
    })?;
    if relative_path.as_os_str().is_empty() {
        return Err(FirecrackerConfigError::invalid(format!(
            "{label} host path must not be the jail root"
        )));
    }
    Ok(Path::new("/")
        .join(relative_path)
        .to_string_lossy()
        .to_string())
}

fn validate_absolute_path(label: &'static str, path: &Path) -> Result<(), FirecrackerConfigError> {
    if path.as_os_str().is_empty() {
        return Err(FirecrackerConfigError::invalid(format!(
            "{label} path is empty"
        )));
    }
    if !path.is_absolute() {
        return Err(FirecrackerConfigError::invalid(format!(
            "{label} path must be absolute"
        )));
    }
    let path = path.to_str().ok_or_else(|| {
        FirecrackerConfigError::invalid(format!("{label} path must be valid UTF-8"))
    })?;
    if path.contains('\0') {
        return Err(FirecrackerConfigError::invalid(format!(
            "{label} path contains a NUL byte"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> FirecrackerConfig {
        FirecrackerConfig::new(
            std::env::temp_dir().join("llm-harness-vmlinux"),
            std::env::temp_dir().join("llm-harness-rootfs.ext4"),
            std::env::temp_dir().join("llm-harness-agent.sock"),
            1,
            128,
            3,
        )
    }

    #[test]
    fn config_generates_firecracker_machine_json() {
        let config = valid_config().with_boot_args("console=ttyS0");
        let value = config.to_json().expect("config json");

        assert_eq!(
            value["boot-source"]["kernel_image_path"],
            std::env::temp_dir()
                .join("llm-harness-vmlinux")
                .to_string_lossy()
                .to_string()
        );
        assert_eq!(value["boot-source"]["boot_args"], "console=ttyS0");
        assert_eq!(value["drives"][0]["is_root_device"], true);
        assert_eq!(value["machine-config"]["vcpu_count"], 1);
        assert_eq!(value["vsock"]["guest_cid"], 3);
    }

    #[test]
    fn config_generates_jailed_machine_json() {
        let jail_root = PathBuf::from("/srv/jailer/firecracker/sandbox-1/root");
        let config = FirecrackerConfig::new(
            jail_root.join("vmlinux"),
            jail_root.join("rootfs.ext4"),
            jail_root.join("guest-agent.sock"),
            1,
            128,
            3,
        )
        .with_boot_args("console=ttyS0");
        let value = config
            .to_jailed_json(&jail_root)
            .expect("jailed config json");

        assert_eq!(value["boot-source"]["kernel_image_path"], "/vmlinux");
        assert_eq!(value["boot-source"]["boot_args"], "console=ttyS0");
        assert_eq!(value["drives"][0]["path_on_host"], "/rootfs.ext4");
        assert_eq!(value["vsock"]["uds_path"], "/guest-agent.sock");
    }

    #[test]
    fn config_rejects_jailed_resources_outside_the_jail_root() {
        let jail_root = PathBuf::from("/srv/jailer/firecracker/sandbox-1/root");
        let config = FirecrackerConfig::new(
            PathBuf::from("/opt/vmlinux"),
            jail_root.join("rootfs.ext4"),
            jail_root.join("guest-agent.sock"),
            1,
            128,
            3,
        );
        let error = config
            .to_jailed_json(&jail_root)
            .expect_err("kernel image outside jail root");
        assert!(
            error
                .to_string()
                .contains("kernel image host path must be inside the jail root"),
            "{error}"
        );
    }

    #[test]
    fn config_rejects_invalid_limits_and_relative_paths() {
        let mut config = valid_config();
        config.vcpu_count = 0;
        assert!(config.to_json().is_err());

        let mut config = valid_config();
        config.vsock_guest_cid = 2;
        assert!(config.to_json().is_err());

        let mut config = valid_config();
        config.kernel_image_path = PathBuf::from("vmlinux");
        assert!(config.to_json().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn config_rejects_non_utf8_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let mut config = valid_config();
        config.kernel_image_path = PathBuf::from(OsString::from_vec(b"/tmp/\xff".to_vec()));
        assert!(config.to_json().is_err());
    }
}
