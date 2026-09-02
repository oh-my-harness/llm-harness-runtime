//! Minimal cgroup v2 enforcement used by the Linux bwrap sandbox.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use llm_harness_platform::sandbox::ResourceLimits;
use tokio::task::spawn_blocking;

const CPU_PERIOD_US: u64 = 100_000;
const EMPTY_WAIT: Duration = Duration::from_secs(2);
const EMPTY_POLL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone)]
pub(crate) struct CgroupV2 {
    path: PathBuf,
}

impl CgroupV2 {
    pub(crate) fn validate(root: &Path, limits: &ResourceLimits) -> Result<()> {
        let required = required_controllers(limits)?;
        if required.is_empty() {
            return Ok(());
        }
        validate_cgroup_v2_root(root)?;
        require_controllers(root, &required)
    }

    pub(crate) fn create(root: &Path, limits: &ResourceLimits) -> Result<Option<Self>> {
        let required = required_controllers(limits)?;
        if required.is_empty() {
            return Ok(None);
        }

        std::fs::create_dir_all(root)?;
        validate_cgroup_v2_root(root)?;
        require_controllers(root, &required)?;

        let path = root.join(format!("llm-harness-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path)?;
        #[cfg(test)]
        create_test_files(&path, limits)?;
        if let Err(error) = write_limits(&path, limits) {
            let _ = std::fs::remove_dir(&path);
            return Err(error);
        }

        Ok(Some(Self { path }))
    }

    pub(crate) fn wrap_bwrap(&self, bwrap_args: &[String]) -> (String, Vec<String>) {
        let procs = self.path.join("cgroup.procs");
        let mut args = vec![
            "-c".to_string(),
            "echo \"$$\" > \"$1\" && shift && exec \"$@\"".to_string(),
            "llm-harness-cgroup".to_string(),
            procs.to_string_lossy().into_owned(),
            "bwrap".to_string(),
        ];
        args.extend(bwrap_args.iter().cloned());
        ("/bin/sh".to_string(), args)
    }

    pub(crate) async fn kill_and_remove(&self) -> Result<()> {
        let path = self.path.clone();
        spawn_blocking(move || kill_and_remove_blocking(&path))
            .await
            .map_err(|error| anyhow!("cgroup cleanup task failed: {error}"))?
    }
}

impl Drop for CgroupV2 {
    fn drop(&mut self) {
        if self.path.exists() {
            let _ = kill_and_remove_blocking(&self.path);
        }
    }
}

fn required_controllers(limits: &ResourceLimits) -> Result<Vec<&'static str>> {
    if limits.max_cpus == Some(0) {
        bail!("max_cpus must be greater than zero");
    }
    if limits.max_processes == Some(0) {
        bail!("max_processes must be greater than zero");
    }
    if limits.max_memory_mb == Some(0) {
        bail!("max_memory_mb must be greater than zero");
    }
    if limits.max_disk_mb.is_some() {
        bail!(
            "max_disk_mb requires a real capacity quota mechanism; \
             BwrapSandbox does not silently substitute I/O throttling"
        );
    }

    let mut required = Vec::new();
    if limits.max_cpus.is_some() {
        required.push("cpu");
    }
    if limits.max_memory_mb.is_some() {
        required.push("memory");
    }
    if limits.max_processes.is_some() {
        required.push("pids");
    }
    Ok(required)
}

#[cfg(test)]
fn create_test_files(path: &Path, limits: &ResourceLimits) -> Result<()> {
    if limits.max_processes.is_some() {
        std::fs::write(path.join("pids.max"), "")?;
    }
    if limits.max_memory_mb.is_some() {
        std::fs::write(path.join("memory.max"), "")?;
    }
    if limits.max_cpus.is_some() {
        std::fs::write(path.join("cpu.max"), "")?;
    }
    Ok(())
}

fn validate_cgroup_v2_root(root: &Path) -> Result<()> {
    for file in [
        "cgroup.controllers",
        "cgroup.subtree_control",
        "cgroup.procs",
    ] {
        if !root.join(file).is_file() {
            bail!(
                "cgroup v2 is unavailable at {}: missing {file}",
                root.display()
            );
        }
    }
    Ok(())
}

fn read_words(path: &Path) -> Result<Vec<String>> {
    Ok(std::fs::read_to_string(path)?
        .split_whitespace()
        .map(str::to_string)
        .collect())
}

fn require_controllers(root: &Path, required: &[&str]) -> Result<()> {
    let available = read_words(&root.join("cgroup.controllers"))?;
    for controller in required {
        if !available.iter().any(|item| item == controller) {
            bail!(
                "cgroup v2 controller '{controller}' is unavailable at {}",
                root.display()
            );
        }
    }

    let enabled = read_words(&root.join("cgroup.subtree_control"))?;
    let missing = required
        .iter()
        .filter(|controller| !enabled.iter().any(|item| item == *controller))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }

    if !missing.is_empty() {
        bail!(
            "cgroup v2 controllers not enabled at {}: {}",
            root.display(),
            missing
                .iter()
                .map(|controller| (*controller).to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

fn write_limits(path: &Path, limits: &ResourceLimits) -> Result<()> {
    if let Some(processes) = limits.max_processes {
        write_file(&path.join("pids.max"), &processes.to_string())?;
    }
    if let Some(memory_mb) = limits.max_memory_mb {
        let bytes = (memory_mb as u64)
            .checked_mul(1024 * 1024)
            .ok_or_else(|| anyhow!("max_memory_mb overflows cgroup memory.max"))?;
        write_file(&path.join("memory.max"), &bytes.to_string())?;
    }
    if let Some(cpus) = limits.max_cpus {
        let quota = (cpus as u64)
            .checked_mul(CPU_PERIOD_US)
            .ok_or_else(|| anyhow!("max_cpus overflows cgroup cpu.max"))?;
        write_file(&path.join("cpu.max"), &format!("{quota} {CPU_PERIOD_US}"))?;
    }
    Ok(())
}

fn write_file(path: &Path, contents: &str) -> Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .write_all(contents.as_bytes())?;
    Ok(())
}

fn kill_and_remove_blocking(path: &Path) -> Result<()> {
    let kill_file = path.join("cgroup.kill");
    if kill_file_exists(path) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(&kill_file)?
            .write_all(b"1")?;
    }

    let deadline = std::time::Instant::now() + EMPTY_WAIT;
    while populated(path)? {
        if std::time::Instant::now() >= deadline {
            bail!("cgroup {} did not become empty", path.display());
        }
        std::thread::sleep(EMPTY_POLL);
    }

    std::fs::remove_dir_all(path)?;
    Ok(())
}

fn kill_file_exists(path: &Path) -> bool {
    path.join("cgroup.kill").is_file()
}

fn populated(path: &Path) -> Result<bool> {
    let events = std::fs::read_to_string(path.join("cgroup.events"))?;
    for line in events.lines() {
        let mut words = line.split_whitespace();
        if words.next() == Some("populated") {
            return Ok(words.next() == Some("1"));
        }
    }
    bail!("cgroup {} has no populated field", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_platform::sandbox::ResourceLimits;

    fn limits(
        max_cpus: Option<usize>,
        max_processes: Option<usize>,
        max_memory_mb: Option<usize>,
    ) -> ResourceLimits {
        ResourceLimits {
            max_cpus,
            max_processes,
            max_memory_mb,
            max_disk_mb: None,
            timeout: None,
        }
    }

    fn fake_cgroup_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("cgroup.controllers"), "cpu memory pids").unwrap();
        std::fs::write(
            root.path().join("cgroup.subtree_control"),
            "cpu memory pids",
        )
        .unwrap();
        std::fs::write(root.path().join("cgroup.procs"), "").unwrap();
        root
    }

    #[test]
    fn no_resource_limits_skips_cgroup() {
        let root = fake_cgroup_root();
        let created = CgroupV2::create(root.path(), &limits(None, None, None)).unwrap();
        assert!(created.is_none());
    }

    #[test]
    fn cgroup_fails_closed_without_v2_root() {
        let root = tempfile::tempdir().unwrap();
        let error = CgroupV2::create(root.path(), &limits(Some(1), None, None)).unwrap_err();
        assert!(error.to_string().contains("cgroup v2 is unavailable"));
    }

    #[test]
    fn cgroup_fails_closed_without_controller() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("cgroup.controllers"), "cpu").unwrap();
        std::fs::write(root.path().join("cgroup.subtree_control"), "").unwrap();
        std::fs::write(root.path().join("cgroup.procs"), "").unwrap();
        let error = CgroupV2::create(root.path(), &limits(None, Some(4), None)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("controller 'pids' is unavailable")
        );
    }

    #[test]
    fn cgroup_writes_configured_limits() {
        let root = fake_cgroup_root();
        let cgroup = CgroupV2::create(root.path(), &limits(Some(2), Some(4), Some(64)))
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(cgroup.path.join("cpu.max")).unwrap(),
            "200000 100000"
        );
        assert_eq!(
            std::fs::read_to_string(cgroup.path.join("pids.max")).unwrap(),
            "4"
        );
        assert_eq!(
            std::fs::read_to_string(cgroup.path.join("memory.max")).unwrap(),
            (64 * 1024 * 1024).to_string()
        );
    }

    #[test]
    fn disk_limit_fails_closed() {
        let root = fake_cgroup_root();
        let mut configured = limits(None, None, None);
        configured.max_disk_mb = Some(1);
        let error = CgroupV2::create(root.path(), &configured).unwrap_err();
        assert!(error.to_string().contains("real capacity quota"));
    }

    #[test]
    fn cgroup_cleanup_removes_empty_cgroup() {
        let root = fake_cgroup_root();
        let cgroup = CgroupV2::create(root.path(), &limits(Some(1), None, None))
            .unwrap()
            .unwrap();
        std::fs::write(cgroup.path.join("cgroup.events"), "populated 0\n").unwrap();
        kill_and_remove_blocking(&cgroup.path).unwrap();
        assert!(!cgroup.path.exists());
    }

    #[test]
    fn dropped_cgroup_removes_empty_cgroup() {
        let root = fake_cgroup_root();
        let cgroup = CgroupV2::create(root.path(), &limits(Some(1), None, None))
            .unwrap()
            .unwrap();
        let path = cgroup.path.clone();
        std::fs::write(path.join("cgroup.events"), "populated 0\n").unwrap();

        drop(cgroup);

        assert!(!path.exists());
    }
}
