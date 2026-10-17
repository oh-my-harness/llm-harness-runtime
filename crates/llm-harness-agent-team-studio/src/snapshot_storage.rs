//! Snapshot policy, conservative run lifecycle, and whole-run retention.
//! Cooperating readers hold an OS shared lease; mutations use an exclusive lease.
use crate::pipeline::ArtifactSnapshot;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

type Result<T> = io::Result<T>;
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureLimits {
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}
impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            max_files: 10_000,
            max_file_bytes: 16 << 20,
            max_total_bytes: 128 << 20,
        }
    }
}
impl CaptureLimits {
    pub fn validate(&self) -> Result<()> {
        if !(1..=100_000).contains(&self.max_files)
            || !(1..=64 << 20).contains(&self.max_file_bytes)
            || !(1..=512 << 20).contains(&self.max_total_bytes)
        {
            return Err(invalid(
                "snapshot capture limits must be positive: max_files <= 100000, max_file_bytes <= 67108864, max_total_bytes <= 536870912",
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionPolicy {
    pub enabled: bool,
    pub count_high: u64,
    pub count_low: u64,
    pub bytes_high: u64,
    pub bytes_low: u64,
}
impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            count_high: 100,
            count_low: 80,
            bytes_high: 10 << 30,
            bytes_low: 7 << 30,
        }
    }
}
impl RetentionPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.count_low == 0
            || self.count_low >= self.count_high
            || self.bytes_low == 0
            || self.bytes_low >= self.bytes_high
            || self.count_high > 1_000_000
            || self.bytes_high > 1 << 50
        {
            return Err(invalid(
                "snapshot retention requires positive low < high watermarks; count_high <= 1000000, bytes_high <= 1125899906842624",
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SnapshotPolicy {
    pub capture: CaptureLimits,
    pub retention: RetentionPolicy,
    /// Reasons to pin every new run (e.g. an isolated E2E/experiment runtime).
    pub protect_new_runs: Vec<String>,
}
impl SnapshotPolicy {
    pub fn validate(&self) -> Result<()> {
        self.capture.validate()?;
        self.retention.validate()?;
        if self.protect_new_runs.len() > 100
            || self
                .protect_new_runs
                .iter()
                .any(|r| r.trim().is_empty() || r.len() > 1024)
        {
            return Err(invalid(
                "protect_new_runs allows at most 100 nonempty reasons, each <= 1024 bytes",
            ));
        }
        Ok(())
    }
    /// Existing global Studio settings; no additional configuration file.
    pub fn load(data_root: &Path) -> Result<Self> {
        match fs::read(data_root.join("settings.json")) {
            Ok(bytes) => {
                let value: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(invalid_json)?;
                Self::from_settings(&value)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }
    pub fn from_settings(value: &serde_json::Value) -> Result<Self> {
        let policy = match value.get("snapshots") {
            Some(v) => serde_json::from_value(v.clone()).map_err(invalid_json)?,
            None => Self::default(),
        };
        policy.validate()?;
        Ok(policy)
    }
}
fn invalid_json(e: serde_json::Error) -> io::Error {
    invalid(format!("invalid snapshot metadata/configuration: {e}"))
}

/// Hold this guard throughout manual Reviewer inspection. Cross-process,
/// kernel-released on crash. Nonblocking acquisition keeps workflows responsive.
pub struct StorageLease {
    _file: File,
}
impl Drop for StorageLease {
    fn drop(&mut self) {
        // Explicit unlock also releases the open-file-description lock when
        // another thread's fork briefly inherits this fd before exec/CLOEXEC.
        let _ = self._file.unlock();
    }
}
impl StorageLease {
    pub fn inspection(repo: &Path) -> Result<Self> {
        Self::acquire(repo, false)
    }
    pub(crate) fn inspection_if_existing(repo: &Path) -> Result<Option<Self>> {
        match fs::symlink_metadata(repo.join(".pipeline")) {
            Ok(_) => Self::inspection(repo).map(Some),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn mutation(repo: &Path) -> Result<Self> {
        Self::acquire(repo, true)
    }
    fn acquire(repo: &Path, exclusive: bool) -> Result<Self> {
        real_dir(repo)?;
        safe_dir(repo, Path::new(".pipeline"))?;
        let path = repo.join(".pipeline/snapshot-storage.lock");
        reject_special(&path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        let result = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        match result {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "snapshot storage lease unavailable: operation would block",
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
        Ok(Self { _file: file })
    }
}
fn real_dir(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.file_type().is_symlink() {
        return Err(invalid(format!(
            "unsafe snapshot directory: {}",
            path.display()
        )));
    }
    Ok(())
}
fn reject_special(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if !m.is_file() || m.file_type().is_symlink() => {
            Err(invalid(format!("unsafe snapshot file: {}", path.display())))
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
fn relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || !path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return Err(invalid("unsafe snapshot relative path"));
    }
    Ok(())
}
fn safe_dir(root: &Path, rel: &Path) -> Result<()> {
    relative(rel)?;
    let mut path = root.to_path_buf();
    for c in rel.components() {
        path.push(c);
        match fs::symlink_metadata(&path) {
            Ok(_) => real_dir(&path)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let builder = fs::DirBuilder::new();
                #[cfg(unix)]
                let mut builder = builder;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder.create(&path)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    reject_special(path)?;
    serde_json::from_slice(&fs::read(path)?).map_err(invalid_json)
}
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    reject_special(path)?;
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(value).map_err(invalid_json)?)?;
    file.sync_all()?;
    fs::rename(tmp, path)?;
    sync_dir(path.parent().expect("metadata parent"))
}
fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunMetadata {
    version: u32,
    run_id: uuid::Uuid,
    created_at: chrono::DateTime<chrono::Utc>,
    completed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Pin the whole chain; reasons include baseline, experiment, and user.
    protections: Vec<String>,
    /// Dependencies protect targets even when the referring run is completed.
    dependencies: Vec<uuid::Uuid>,
}
fn run_root(repo: &Path, run: uuid::Uuid) -> PathBuf {
    repo.join(".pipeline/snapshots").join(run.to_string())
}
/// Caller owns a workflow shared lease before starting capture.
pub fn begin_run(repo: &Path, run: uuid::Uuid) -> Result<()> {
    begin_run_protected(repo, run, &[])
}
pub fn begin_run_protected(repo: &Path, run: uuid::Uuid, reasons: &[String]) -> Result<()> {
    let policy = SnapshotPolicy {
        protect_new_runs: reasons.to_vec(),
        ..Default::default()
    };
    policy.validate()?;
    let _lease = StorageLease::inspection(repo)?;
    let root = run_root(repo, run);
    if root.exists() {
        return Err(invalid(
            "snapshot run already exists; refusing to replace lifecycle metadata",
        ));
    }
    safe_dir(
        repo,
        &Path::new(".pipeline/snapshots").join(run.to_string()),
    )?;
    atomic_json(
        &root.join("retention.json"),
        &RunMetadata {
            version: 1,
            run_id: run,
            created_at: chrono::Utc::now(),
            completed_at: None,
            protections: reasons.to_vec(),
            dependencies: Vec::new(),
        },
    )
}
pub fn complete_run(repo: &Path, run: uuid::Uuid) -> Result<()> {
    let _lease = StorageLease::inspection(repo)?;
    real_dir(&repo.join(".pipeline/snapshots"))?;
    let root = run_root(repo, run);
    real_dir(&root)?;
    let mut metadata: RunMetadata = read_json(&root.join("retention.json"))?;
    if metadata.version != 1 || metadata.run_id != run {
        return Err(invalid("inconsistent snapshot lifecycle metadata"));
    }
    validate_chain(repo, &root, run)?;
    metadata.completed_at = Some(chrono::Utc::now());
    atomic_json(&root.join("retention.json"), &metadata)
}
/// Explicit run protection; no adoption/unprotection of unknown legacy evidence.
pub fn protect_run(repo: &Path, run: uuid::Uuid, reason: &str, protect: bool) -> Result<()> {
    if reason.trim().is_empty() {
        return Err(invalid("snapshot protection requires a nonempty reason"));
    }
    let _lease = StorageLease::mutation(repo)?;
    real_dir(&repo.join(".pipeline/snapshots"))?;
    let root = run_root(repo, run);
    real_dir(&root)?;
    let mut metadata: RunMetadata = read_json(&root.join("retention.json"))?;
    if metadata.version != 1 || metadata.run_id != run {
        return Err(invalid("inconsistent snapshot lifecycle metadata"));
    }
    metadata.protections.retain(|r| r != reason);
    if protect {
        metadata.protections.push(reason.into());
    }
    atomic_json(&root.join("retention.json"), &metadata)
}
/// Register a historical dependency without changing the workflow protocol.
pub fn set_run_dependency(
    repo: &Path,
    owner: uuid::Uuid,
    target: uuid::Uuid,
    required: bool,
) -> Result<()> {
    if owner == target {
        return Err(invalid("a snapshot run cannot depend on itself"));
    }
    let _lease = StorageLease::mutation(repo)?;
    real_dir(&repo.join(".pipeline/snapshots"))?;
    let root = run_root(repo, owner);
    real_dir(&root)?;
    if required {
        real_dir(&run_root(repo, target))?;
    }
    let mut metadata: RunMetadata = read_json(&root.join("retention.json"))?;
    if metadata.version != 1 || metadata.run_id != owner {
        return Err(invalid("inconsistent snapshot lifecycle metadata"));
    }
    metadata.dependencies.retain(|id| *id != target);
    if required {
        metadata.dependencies.push(target);
    }
    atomic_json(&root.join("retention.json"), &metadata)
}
fn allocated_bytes(metadata: &fs::Metadata) -> Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata
            .blocks()
            .checked_mul(512)
            .ok_or_else(|| invalid("allocated snapshot bytes overflow"))
    }
    #[cfg(not(unix))]
    {
        Ok(metadata.len())
    }
}
fn tree_bytes(root: &Path) -> Result<u64> {
    real_dir(root)?;
    let mut bytes = allocated_bytes(&fs::symlink_metadata(root)?)?;
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        let m = fs::symlink_metadata(&path)?;
        if m.file_type().is_symlink() {
            return Err(invalid(format!(
                "symlink in snapshot storage: {}",
                path.display()
            )));
        }
        let size = if m.is_dir() {
            tree_bytes(&path)?
        } else if m.is_file() {
            allocated_bytes(&m)?
        } else {
            return Err(invalid("special file in snapshot storage"));
        };
        bytes = bytes
            .checked_add(size)
            .ok_or_else(|| invalid("snapshot byte count overflow"))?;
    }
    Ok(bytes)
}
fn validate_chain(repo: &Path, root: &Path, run: uuid::Uuid) -> Result<u64> {
    let mut rounds = BTreeSet::new();
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.file_name().is_some_and(|n| n == "retention.json") {
            continue;
        }
        real_dir(&path)?;
        let name = path.file_name().unwrap().to_string_lossy();
        let round: u32 = name
            .strip_prefix('r')
            .ok_or_else(|| invalid("unknown snapshot run entry"))?
            .parse()
            .map_err(|_| invalid("invalid snapshot round"))?;
        if name != format!("r{round}") {
            return Err(invalid("noncanonical snapshot round"));
        }
        let s: ArtifactSnapshot = read_json(&path.join("snapshot.json"))?;
        if s.run_id != run.to_string()
            || s.round != round
            || s.evidence_dir != path
            || s.repo_root != repo
        {
            return Err(invalid("snapshot manifest does not match storage location"));
        }
        if round > 0 && s.task_start_baseline.as_ref() != Some(&root.join("r0/snapshot.json")) {
            return Err(invalid(
                "snapshot baseline is missing or references another run",
            ));
        }
        s.capture_limits.validate()?;
        if round == 0 && s.task_start_baseline.is_some() {
            return Err(invalid("task-start snapshot cannot reference a baseline"));
        }
        real_dir(&path.join("files"))?;
        let unique_paths: BTreeSet<_> = s.changed_files.iter().collect();
        if unique_paths.len() != s.changed_files.len()
            || s.changed_files.len() != s.file_hashes.len()
            || s.file_hashes
                .values()
                .any(|hash| hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(invalid("inconsistent snapshot file inventory"));
        }
        for rel in &s.changed_files {
            relative(Path::new(rel))?;
            if !s.file_hashes.contains_key(rel) {
                return Err(invalid("snapshot hash missing"));
            }
            let file = path.join("files").join(rel);
            reject_special(&file)?;
            if !file.is_file() {
                return Err(invalid("snapshot file missing"));
            }
        }
        rounds.insert(round);
    }
    if rounds.is_empty() || rounds.iter().copied().ne(0..rounds.len() as u32) {
        return Err(invalid("incomplete snapshot review chain"));
    }
    Ok(rounds.len() as u64)
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedRun {
    pub run_id: uuid::Uuid,
    pub snapshots: u64,
    pub bytes: u64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    pub snapshots: u64,
    pub bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupReport {
    pub version: u32,
    pub operation: uuid::Uuid,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub dry_run: bool,
    pub policy: RetentionPolicy,
    pub triggers: Vec<String>,
    pub usage_known: bool,
    pub before: Usage,
    pub after: Usage,
    pub selected: Vec<SelectedRun>,
    pub skipped: BTreeMap<String, String>,
    pub archived_paths: Vec<PathBuf>,
    pub removed_paths: Vec<PathBuf>,
    pub reclaimed_bytes: u64,
    pub errors: Vec<String>,
    pub incomplete: bool,
    /// Durable authorization for detached trash recovery.
    pub phase: String,
}
struct Inventory {
    usage: Usage,
    candidates: Vec<(chrono::DateTime<chrono::Utc>, SelectedRun)>,
    skipped: BTreeMap<String, String>,
}
fn inventory(repo: &Path) -> Result<Inventory> {
    let root = repo.join(".pipeline/snapshots");
    let mut out = Inventory {
        usage: Usage::default(),
        candidates: Vec::new(),
        skipped: BTreeMap::new(),
    };
    if !root.exists() {
        return Ok(out);
    }
    real_dir(&root)?;
    let mut dependencies = BTreeSet::new();
    for entry in fs::read_dir(&root)? {
        let path = entry?.path();
        real_dir(&path)?;
        let id = path.file_name().unwrap().to_string_lossy().to_string();
        let bytes = tree_bytes(&path)?;
        let mut count = 0;
        // Read *every* retained manifest, including legacy runs, so baseline
        // references cannot dangle after deletion of a candidate run.
        for round in fs::read_dir(&path)? {
            let round = round?.path();
            if round.is_dir() {
                let manifest = round.join("snapshot.json");
                if manifest.exists() {
                    let s: ArtifactSnapshot = read_json(&manifest)?;
                    count += 1;
                    if let Some(baseline) = s.task_start_baseline {
                        if let Some(target) = baseline
                            .parent()
                            .and_then(Path::parent)
                            .and_then(Path::file_name)
                            .and_then(|n| n.to_str())
                        {
                            if target != id {
                                dependencies.insert(target.to_owned());
                            }
                        } else {
                            return Err(invalid("unresolvable snapshot baseline dependency"));
                        }
                    }
                }
            }
        }
        out.usage.snapshots += count;
        out.usage.bytes = out
            .usage
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| invalid("snapshot usage overflow"))?;
        let metadata = read_json::<RunMetadata>(&path.join("retention.json"));
        // Even an unknown version or mismatched owner can declare a valid
        // dependency. Corrupt metadata may hide dependencies: fail the whole
        // inventory rather than guessing that other runs are reclaimable.
        match &metadata {
            Ok(m) => dependencies.extend(m.dependencies.iter().map(ToString::to_string)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(invalid(format!(
                    "cannot establish retention dependencies for {id}: {e}"
                )));
            }
        }
        match metadata {
            Ok(m) if m.version == 1 && m.run_id.to_string() == id => {
                dependencies.extend(m.dependencies.iter().map(ToString::to_string));
                let reason = if m.completed_at.is_none() {
                    Some("active or interrupted workflow")
                } else if m.completed_at.is_some_and(|t| t < m.created_at) {
                    Some("inconsistent lifecycle times")
                } else if !m.protections.is_empty() {
                    Some("explicitly protected evidence")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    out.skipped.insert(id, reason.into());
                } else {
                    match validate_chain(repo, &path, m.run_id) {
                        Ok(n) if n == count => out.candidates.push((
                            m.created_at,
                            SelectedRun {
                                run_id: m.run_id,
                                snapshots: n,
                                bytes,
                            },
                        )),
                        _ => {
                            out.skipped
                                .insert(id, "incomplete or inconsistent evidence chain".into());
                        }
                    }
                }
            }
            _ => {
                out.skipped.insert(
                    id,
                    "unknown, legacy, or inconsistent protection metadata".into(),
                );
            }
        }
    }
    out.candidates.retain(|(_, run)| {
        let id = run.run_id.to_string();
        if dependencies.contains(&id) {
            out.skipped
                .insert(id, "required by retained evidence dependency".into());
            false
        } else {
            true
        }
    });
    out.candidates
        .sort_by_key(|(time, run)| (*time, run.run_id));
    Ok(out)
}
fn journal_path(repo: &Path, operation: uuid::Uuid) -> PathBuf {
    repo.join(".pipeline/snapshot-retention")
        .join(format!("{operation}.json"))
}
fn trash_root(repo: &Path, operation: uuid::Uuid) -> PathBuf {
    repo.join(".pipeline/snapshot-trash")
        .join(operation.to_string())
}
fn persist(repo: &Path, report: &CleanupReport) -> Result<()> {
    safe_dir(repo, Path::new(".pipeline/snapshot-retention"))?;
    atomic_json(&journal_path(repo, report.operation), report)?;
    tracing::info!(operation = %report.operation, phase = %report.phase, reclaimed_bytes = report.reclaimed_bytes, incomplete = report.incomplete, "snapshot retention record");
    Ok(())
}
/// Recover only trash backed by a durable, validated deletion intent. Never
/// infer authorization from an orphan directory; never resume deleting sources.
fn recover(repo: &Path) -> Result<()> {
    let root = repo.join(".pipeline/snapshot-trash");
    if !root.exists() {
        return Ok(());
    }
    real_dir(&root)?;
    for entry in fs::read_dir(&root)? {
        let trash = entry?.path();
        real_dir(&trash)?;
        let operation: uuid::Uuid = trash
            .file_name()
            .unwrap()
            .to_string_lossy()
            .parse()
            .map_err(|_| invalid("unknown retention trash; manual inspection required"))?;
        let mut report: CleanupReport = read_json(&journal_path(repo, operation))?;
        report.policy.validate()?;
        let unique: BTreeSet<_> = report.selected.iter().map(|r| r.run_id).collect();
        let authorized_trigger = (report.before.snapshots >= report.policy.count_high
            && report.triggers.contains(&"count".into()))
            || (report.before.bytes >= report.policy.bytes_high
                && report.triggers.contains(&"bytes".into()));
        if trash.file_name().unwrap().to_string_lossy() != operation.to_string()
            || report.version != 1
            || report.operation != operation
            || report.dry_run
            || report.phase != "removing"
            || !report.policy.enabled
            || !report.usage_known
            || !authorized_trigger
            || unique.len() != report.selected.len()
            || report.selected.is_empty()
            || report.selected.iter().any(|r| {
                r.snapshots == 0
                    || r.bytes == 0
                    || report.skipped.contains_key(&r.run_id.to_string())
            })
        {
            return Err(invalid(
                "trash lacks valid retention deletion authorization",
            ));
        }
        for entry in fs::read_dir(&trash)? {
            let path = entry?.path();
            let id: uuid::Uuid = path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .parse()
                .map_err(|_| invalid("unknown retained trash entry"))?;
            if !report.selected.iter().any(|s| s.run_id == id) {
                return Err(invalid("trash run not selected in cleanup record"));
            }
            let bytes = tree_bytes(&path)?;
            if !report.archived_paths.contains(&path) {
                report.archived_paths.push(path.clone());
            }
            fs::remove_dir_all(&path)?;
            report.reclaimed_bytes += bytes;
            report.removed_paths.push(path);
            persist(repo, &report)?;
        }
        fs::remove_dir(&trash)?;
        sync_dir(&root)?;
        report.reclaimed_bytes = report
            .selected
            .iter()
            .filter(|r| !run_root(repo, r.run_id).exists())
            .map(|r| r.bytes)
            .sum();
        report.phase = "recovered".into();
        report.after = inventory(repo)?.usage;
        report.incomplete = report.after.snapshots > report.policy.count_low
            || report.after.bytes > report.policy.bytes_low;
        persist(repo, &report)?;
    }
    Ok(())
}
/// Whole-run hysteresis: either high triggers; both lows are targets. Disabled
/// policies still allow explicit dry-run planning, but never real deletion.
pub fn retain(repo: &Path, policy: &RetentionPolicy, dry_run: bool) -> Result<CleanupReport> {
    policy.validate()?;
    let _lease = StorageLease::mutation(repo)?;
    let recovery_error = if !dry_run && policy.enabled {
        recover(repo).err()
    } else {
        None
    };
    let current = inventory(repo);
    let mut report = CleanupReport {
        version: 1,
        operation: uuid::Uuid::new_v4(),
        timestamp: chrono::Utc::now(),
        dry_run,
        policy: policy.clone(),
        triggers: Vec::new(),
        usage_known: false,
        before: Usage::default(),
        after: Usage::default(),
        selected: Vec::new(),
        skipped: BTreeMap::new(),
        archived_paths: Vec::new(),
        removed_paths: Vec::new(),
        reclaimed_bytes: 0,
        errors: Vec::new(),
        incomplete: false,
        phase: "planned".into(),
    };
    if let Some(error) = recovery_error {
        report.errors.push(error.to_string());
        report.incomplete = true;
        report.phase = "aborted".into();
        persist(repo, &report)?;
        return Ok(report);
    }
    let current = match current {
        Ok(current) => current,
        Err(error) => {
            report.errors.push(error.to_string());
            report.incomplete = true;
            report.phase = "aborted".into();
            persist(repo, &report)?;
            return Ok(report);
        }
    };
    report.usage_known = true;
    report.before = current.usage.clone();
    report.after = current.usage;
    report.skipped = current.skipped;
    if report.before.snapshots >= policy.count_high {
        report.triggers.push("count".into());
    }
    if report.before.bytes >= policy.bytes_high {
        report.triggers.push("bytes".into());
    }
    if !report.triggers.is_empty() && (policy.enabled || dry_run) {
        for (_, run) in current.candidates {
            if report.after.snapshots <= policy.count_low && report.after.bytes <= policy.bytes_low
            {
                break;
            }
            report.after.snapshots -= run.snapshots;
            report.after.bytes -= run.bytes;
            report.selected.push(run);
        }
        report.incomplete =
            report.after.snapshots > policy.count_low || report.after.bytes > policy.bytes_low;
    }
    if dry_run || !policy.enabled || report.selected.is_empty() {
        report.phase = if dry_run { "dry_run" } else { "unchanged" }.into();
        if !dry_run {
            report.after = report.before.clone();
        }
        persist(repo, &report)?;
        return Ok(report);
    }
    report.after = report.before.clone();
    report.phase = "removing".into();
    persist(repo, &report)?; // fsync intent *before* detach/delete.
    let trash = trash_root(repo, report.operation);
    safe_dir(
        repo,
        &Path::new(".pipeline/snapshot-trash").join(report.operation.to_string()),
    )?;
    for run in report.selected.clone() {
        let source = run_root(repo, run.run_id);
        let target = trash.join(run.run_id.to_string());
        let deletion = (|| -> Result<()> {
            real_dir(&source)?;
            tree_bytes(&source)?;
            fs::rename(&source, &target)?;
            sync_dir(source.parent().unwrap())?;
            sync_dir(&trash)?;
            report.archived_paths.push(target.clone());
            persist(repo, &report)?;
            fs::remove_dir_all(&target)?;
            sync_dir(&trash)?;
            Ok(())
        })();
        match deletion {
            Ok(()) => {
                report.reclaimed_bytes += run.bytes;
                report.removed_paths.push(source);
            }
            Err(e) => {
                report.errors.push(format!("{}: {e}", run.run_id));
                report.incomplete = true;
                report.after = inventory(repo)?.usage;
                persist(repo, &report)?;
                return Ok(report);
            }
        }
        persist(repo, &report)?;
    }
    fs::remove_dir(&trash)?;
    sync_dir(trash.parent().unwrap())?;
    report.after = inventory(repo)?.usage;
    report.phase = "completed".into();
    report.incomplete =
        report.after.snapshots > policy.count_low || report.after.bytes > policy.bytes_low;
    persist(repo, &report)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;
    fn policy(high: u64, low: u64) -> RetentionPolicy {
        RetentionPolicy {
            enabled: true,
            count_high: high,
            count_low: low,
            bytes_high: 1 << 30,
            bytes_low: 1 << 29,
        }
    }
    fn run(repo: &Path, rounds: u32, payload: usize, completed: bool) -> uuid::Uuid {
        let id = uuid::Uuid::new_v4();
        begin_run(repo, id).unwrap();
        let root = run_root(repo, id);
        for round in 0..rounds {
            let evidence = root.join(format!("r{round}"));
            fs::create_dir_all(evidence.join("files")).unwrap();
            let bytes = vec![42u8; payload];
            fs::write(evidence.join("files/source.bin"), &bytes).unwrap();
            let snapshot = serde_json::json!({
                "issue_id": "synthetic", "run_id": id.to_string(), "round": round, "repo_root": repo,
                "changed_files": ["source.bin"], "capture_policy": "filesystem_no_git", "excluded_ignored_paths": [],
                "file_hashes": {"source.bin": format!("{:x}", Sha256::digest(&bytes))}, "diff": null, "dirty": null,
                "content_hash": "synthetic", "snapshot_id": format!("synthetic#r{round}"), "evidence_dir": evidence,
                "task_start_baseline": if round > 0 { Some(root.join("r0/snapshot.json")) } else { None },
            });
            atomic_json(&evidence.join("snapshot.json"), &snapshot).unwrap();
        }
        if completed {
            complete_run(repo, id).unwrap();
        }
        id
    }
    #[test]
    fn configuration_is_strict_and_defaults_compatible() {
        let p = SnapshotPolicy::default();
        assert_eq!(p.capture.max_files, 10_000);
        assert_eq!(p.capture.max_file_bytes, 16 << 20);
        assert_eq!(p.capture.max_total_bytes, 128 << 20);
        assert!(!p.retention.enabled);
        for value in [
            serde_json::json!({"capture":{"max_files":0}}),
            serde_json::json!({"capture":{"max_total_bytes": (512u64<<20)+1}}),
            serde_json::json!({"capture":{"max_file_bytes":-1}}),
            serde_json::json!({"retention":{"count_low":100}}),
            serde_json::json!({"retention":{"bytes_low":10u64<<30}}),
            serde_json::json!({"retention":{"enabled":"true"}}),
            serde_json::json!({"capture":{"misspelled":1}}),
            serde_json::Value::Null,
        ] {
            assert!(
                SnapshotPolicy::from_settings(&serde_json::json!({"snapshots":value})).is_err()
            );
        }
        let large = SnapshotPolicy::from_settings(
            &serde_json::json!({"snapshots":{"capture":{"max_total_bytes":512u64<<20}}}),
        )
        .unwrap();
        assert_eq!(large.capture.max_total_bytes, 512 << 20);
    }
    #[test]
    fn count_101_cleans_to_80_and_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let ids: Vec<_> = (0..101).map(|_| run(repo, 1, 3, true)).collect();
        let report = retain(repo, &policy(100, 80), false).unwrap();
        assert_eq!(report.before.snapshots, 101);
        assert_eq!(report.after.snapshots, 80);
        assert_eq!(report.selected.len(), 21);
        assert_eq!(report.triggers, ["count"]);
        assert!(!report.incomplete);
        assert!(!run_root(repo, ids[0]).exists());
        assert!(run_root(repo, ids[100]).exists());
        let again = retain(repo, &policy(100, 80), false).unwrap();
        assert!(again.selected.is_empty());
        assert_eq!(again.after.snapshots, 80);
    }
    #[test]
    fn watermark_boundaries_and_disabled_policy() {
        for count in [79, 90, 99, 100, 120] {
            let tmp = TempDir::new().unwrap();
            for _ in 0..count {
                run(tmp.path(), 1, 1, true);
            }
            let report = retain(tmp.path(), &policy(100, 80), false).unwrap();
            assert_eq!(
                report.after.snapshots,
                if count >= 100 { 80 } else { count }
            );
            assert_eq!(report.selected.is_empty(), count < 100);
        }
        let tmp = TempDir::new().unwrap();
        for _ in 0..5 {
            run(tmp.path(), 1, 1, true);
        }
        let mut p = policy(5, 3);
        p.enabled = false;
        assert!(retain(tmp.path(), &p, false).unwrap().selected.is_empty());
        assert_eq!(retain(tmp.path(), &p, true).unwrap().selected.len(), 2);
        assert_eq!(inventory(tmp.path()).unwrap().usage.snapshots, 5);
    }
    #[test]
    fn bytes_and_both_triggers_aim_for_both_lows() {
        for count_pressure in [false, true] {
            let tmp = TempDir::new().unwrap();
            for _ in 0..5 {
                run(tmp.path(), 1, 4000, true);
            }
            let total = inventory(tmp.path()).unwrap().usage.bytes;
            let mut p = policy(
                if count_pressure { 5 } else { 100 },
                if count_pressure { 3 } else { 80 },
            );
            p.bytes_high = total;
            p.bytes_low = total / 2;
            let report = retain(tmp.path(), &p, false).unwrap();
            assert_eq!(report.triggers.contains(&"count".into()), count_pressure);
            assert!(report.triggers.contains(&"bytes".into()));
            assert!(report.after.bytes <= p.bytes_low);
            assert!(report.after.snapshots <= p.count_low);
            assert_eq!(report.selected.len(), 3);
            assert_eq!(
                report.reclaimed_bytes,
                report.before.bytes - report.after.bytes
            );
        }
    }
    #[test]
    fn active_pinned_legacy_and_dependencies_preserved() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let active = run(repo, 1, 1, false);
        let pinned = run(repo, 1, 1, true);
        let baseline = run(repo, 1, 1, true);
        let legacy = run(repo, 1, 1, true);
        let ordinary = run(repo, 1, 1, true);
        protect_run(repo, pinned, "E2E baseline", true).unwrap();
        fs::remove_file(run_root(repo, legacy).join("retention.json")).unwrap();
        let mut metadata: RunMetadata =
            read_json(&run_root(repo, pinned).join("retention.json")).unwrap();
        metadata.dependencies.push(baseline);
        atomic_json(&run_root(repo, pinned).join("retention.json"), &metadata).unwrap();
        let report = retain(repo, &policy(5, 1), false).unwrap();
        assert!(report.incomplete);
        assert_eq!(report.after.snapshots, 4);
        assert_eq!(report.skipped.len(), 4);
        assert!(!run_root(repo, ordinary).exists());
        for id in [active, pinned, baseline, legacy] {
            assert!(run_root(repo, id).exists());
        }
        assert!(protect_run(repo, legacy, "legacy", false).is_err());
    }
    #[test]
    fn complete_chains_removed_together_and_incomplete_chains_skipped() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let first = run(repo, 3, 10, true);
        let broken = run(repo, 3, 10, true);
        let last = run(repo, 3, 10, true);
        fs::remove_file(run_root(repo, broken).join("r1/snapshot.json")).unwrap();
        let report = retain(repo, &policy(8, 5), false).unwrap();
        assert_eq!(report.selected.len(), 1);
        assert_eq!(report.selected[0].run_id, first);
        assert_eq!(report.after.snapshots, 5);
        assert!(!run_root(repo, first).exists());
        assert!(run_root(repo, broken).join("r0/snapshot.json").exists());
        assert!(run_root(repo, last).join("r2/snapshot.json").exists());
        // A parseable but contradictory file inventory also stays protected.
        let other = TempDir::new().unwrap();
        let repo = other.path();
        let corrupt = run(repo, 1, 10, true);
        let ordinary = run(repo, 1, 10, true);
        let manifest = run_root(repo, corrupt).join("r0/snapshot.json");
        let mut value: serde_json::Value = read_json(&manifest).unwrap();
        value["changed_files"] = serde_json::json!(["source.bin", "source.bin"]);
        value["file_hashes"]["missing.bin"] = value["file_hashes"]["source.bin"].clone();
        atomic_json(&manifest, &value).unwrap();
        let report = retain(repo, &policy(2, 1), false).unwrap();
        assert!(run_root(repo, corrupt).exists());
        assert!(!run_root(repo, ordinary).exists());
        assert_eq!(report.skipped.len(), 1);
    }
    #[test]
    fn dry_run_preserves_all_bytes_and_records_plan() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        for _ in 0..5 {
            run(repo, 1, 1, true);
        }
        let report = retain(repo, &policy(5, 3), true).unwrap();
        assert_eq!(report.selected.len(), 2);
        assert_eq!(report.after.snapshots, 3);
        assert_eq!(report.reclaimed_bytes, 0);
        assert!(report.removed_paths.is_empty());
        assert_eq!(inventory(repo).unwrap().usage.snapshots, 5);
        assert!(journal_path(repo, report.operation).is_file());
        assert!(!repo.join(".pipeline/snapshot-trash").exists());
    }
    #[test]
    fn capture_and_inspection_lease_blocks_cleanup_across_threads() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        for _ in 0..5 {
            run(repo, 1, 1, true);
        }
        let active = uuid::Uuid::new_v4();
        begin_run(repo, active).unwrap();
        fs::write(repo.join("source.blend"), vec![1u8; 2 << 20]).unwrap();
        let lease = StorageLease::inspection(repo).unwrap();
        let capture_repo = repo.to_path_buf();
        let capture = std::thread::spawn(move || {
            ArtifactSnapshot::collect(&capture_repo, "active", active, 0).unwrap()
        });
        let owned = repo.to_path_buf();
        let error = std::thread::spawn(move || retain(&owned, &policy(5, 3), false).unwrap_err())
            .join()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        capture.join().unwrap();
        assert_eq!(inventory(repo).unwrap().usage.snapshots, 6);
        drop(lease);
        assert_eq!(
            retain(repo, &policy(5, 3), false).unwrap().after.snapshots,
            3
        );
        assert!(run_root(repo, active).join("r0/snapshot.json").exists());
        assert_eq!(fs::read(repo.join("source.blend")).unwrap().len(), 2 << 20);
    }
    #[tokio::test]
    async fn workflow_waits_for_in_progress_maintenance() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let maintenance = StorageLease::mutation(repo).unwrap();
        let controller = crate::pipeline::PipelineController::new(
            "lease-test",
            llm_harness_agent_team::SocialContext::new(10),
            repo.to_path_buf(),
            &[],
            1,
        );
        let mut task =
            tokio::spawn(async move { controller.snapshot_workflow_lease().await.unwrap() });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(60), &mut task)
                .await
                .is_err()
        );
        drop(maintenance);
        let lease = tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        drop(lease);
    }
    #[test]
    fn interrupted_detach_and_partial_deletion_recovered_without_deleting_sources() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        for _ in 0..5 {
            run(repo, 1, 50, true);
        }
        let mut report = retain(repo, &policy(5, 3), true).unwrap();
        report.dry_run = false;
        report.phase = "removing".into();
        persist(repo, &report).unwrap();
        let trash = trash_root(repo, report.operation);
        safe_dir(
            repo,
            &Path::new(".pipeline/snapshot-trash").join(report.operation.to_string()),
        )
        .unwrap();
        let first = &report.selected[0];
        let target = trash.join(first.run_id.to_string());
        fs::rename(run_root(repo, first.run_id), &target).unwrap();
        fs::remove_file(target.join("retention.json")).unwrap();
        let untouched = report.selected[1].run_id;
        let dry = retain(repo, &policy(5, 3), true).unwrap();
        assert!(target.exists());
        assert!(dry.selected.is_empty());
        let next = retain(repo, &policy(5, 3), false).unwrap();
        assert!(next.selected.is_empty());
        assert!(!target.exists());
        assert!(run_root(repo, untouched).exists());
        let recovered: CleanupReport = read_json(&journal_path(repo, report.operation)).unwrap();
        assert_eq!(recovered.phase, "recovered");
        assert_eq!(recovered.after.snapshots, 4);
        assert_eq!(
            retain(repo, &policy(5, 3), false).unwrap().after.snapshots,
            4
        );
    }
    #[test]
    fn orphan_trash_and_malformed_manifest_fail_closed() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let id = run(repo, 1, 10, true);
        run(repo, 1, 10, true);
        fs::write(run_root(repo, id).join("r0/snapshot.json"), "{}").unwrap();
        assert!(!retain(repo, &policy(2, 1), false).unwrap().usage_known);
        assert!(run_root(repo, id).exists());
        let orphan = uuid::Uuid::new_v4();
        safe_dir(
            repo,
            &Path::new(".pipeline/snapshot-trash").join(orphan.to_string()),
        )
        .unwrap();
        fs::write(trash_root(repo, orphan).join("important"), "preserve").unwrap();
        assert!(retain(repo, &policy(2, 1), false).unwrap().incomplete);
        assert!(trash_root(repo, orphan).join("important").exists());
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_never_delete_external_or_workspace_files() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let repo = tmp.path();
        let id = run(repo, 1, 10, true);
        run(repo, 1, 10, true);
        fs::write(outside.path().join("precious"), "external").unwrap();
        fs::write(repo.join("source.blend"), "workspace").unwrap();
        symlink(outside.path(), run_root(repo, id).join("outside")).unwrap();
        assert!(retain(repo, &policy(2, 1), false).unwrap().incomplete);
        assert_eq!(
            fs::read_to_string(outside.path().join("precious")).unwrap(),
            "external"
        );
        assert_eq!(
            fs::read_to_string(repo.join("source.blend")).unwrap(),
            "workspace"
        );
    }
    #[test]
    fn configurable_capture_file_count_file_size_and_aggregate_limits() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        fs::write(repo.join("a"), b"1234").unwrap();
        fs::write(repo.join("b"), b"12345").unwrap();
        for limits in [
            CaptureLimits {
                max_files: 1,
                ..Default::default()
            },
            CaptureLimits {
                max_file_bytes: 4,
                ..Default::default()
            },
            CaptureLimits {
                max_total_bytes: 8,
                ..Default::default()
            },
        ] {
            let id = uuid::Uuid::new_v4();
            let error = ArtifactSnapshot::collect_configured(repo, "test", id, 0, None, &limits)
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("snapshot capture limit exceeded")
            );
            assert!(!run_root(repo, id).join("r0/snapshot.json").exists());
        }
        let limits = CaptureLimits {
            max_files: 2,
            max_file_bytes: 5,
            max_total_bytes: 9,
        };
        let snapshot = ArtifactSnapshot::collect_configured(
            repo,
            "test",
            uuid::Uuid::new_v4(),
            0,
            None,
            &limits,
        )
        .unwrap();
        assert_eq!(snapshot.changed_files.len(), 2);
        assert_eq!(
            fs::read(snapshot.evidence_dir.join("files/a")).unwrap(),
            b"1234"
        );
    }
    #[test]
    fn settings_updates_preserve_policy_and_reject_invalid_policy() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.json");
        let policy = serde_json::json!({"capture":{"max_total_bytes":512u64<<20},"retention":{"enabled":true}});
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"snapshots":policy})).unwrap(),
        )
        .unwrap();
        crate::settings::Settings::default()
            .save(tmp.path())
            .unwrap();
        assert_eq!(
            SnapshotPolicy::load(tmp.path())
                .unwrap()
                .capture
                .max_total_bytes,
            512 << 20
        );
        let json: serde_json::Value = read_json(&path).unwrap();
        assert_eq!(json["snapshots"], policy);
        fs::write(&path, r#"{"snapshots":{"capture":{"max_files":0}}}"#).unwrap();
        assert!(crate::settings::Settings::load(tmp.path()).is_err());
        assert!(
            crate::settings::Settings::default()
                .save(tmp.path())
                .is_err()
        );
    }
    #[test]
    fn new_benchmark_runs_are_pinned_before_capture_and_legacy_dependencies_protect_targets() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let id = uuid::Uuid::new_v4();
        begin_run_protected(repo, id, &["E2E benchmark".into()]).unwrap();
        let m: RunMetadata = read_json(&run_root(repo, id).join("retention.json")).unwrap();
        assert_eq!(m.protections, ["E2E benchmark"]);
        let target = run(repo, 1, 10, true);
        let legacy = run(repo, 2, 10, true);
        fs::remove_file(run_root(repo, legacy).join("retention.json")).unwrap();
        let path = run_root(repo, legacy).join("r1/snapshot.json");
        let mut value: serde_json::Value = read_json(&path).unwrap();
        value["task_start_baseline"] =
            serde_json::json!(run_root(repo, target).join("r0/snapshot.json"));
        atomic_json(&path, &value).unwrap();
        let report = retain(repo, &policy(3, 1), false).unwrap();
        assert!(report.incomplete);
        assert!(report.selected.is_empty());
        assert!(run_root(repo, target).exists());
        assert!(
            SnapshotPolicy::from_settings(
                &serde_json::json!({"snapshots":{"protect_new_runs":[""]}})
            )
            .is_err()
        );
    }
    #[test]
    fn unknown_versions_preserve_dependencies_and_corrupt_metadata_aborts_cleanup() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let target = run(repo, 1, 10, true);
        let owner = run(repo, 1, 10, true);
        set_run_dependency(repo, owner, target, true).unwrap();
        let path = run_root(repo, owner).join("retention.json");
        let mut m: RunMetadata = read_json(&path).unwrap();
        m.version = 99;
        atomic_json(&path, &m).unwrap();
        let report = retain(repo, &policy(2, 1), false).unwrap();
        assert!(report.selected.is_empty());
        assert!(run_root(repo, target).exists());
        fs::write(&path, "{").unwrap();
        let report = retain(repo, &policy(2, 1), false).unwrap();
        assert!(!report.usage_known);
        assert_eq!(report.phase, "aborted");
        assert!(run_root(repo, target).exists());
    }
    #[cfg(unix)]
    #[test]
    fn protection_operations_reject_symlinked_storage_parents() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let repo = tmp.path();
        let id = run(repo, 1, 10, true);
        let original = fs::read(run_root(repo, id).join("retention.json")).unwrap();
        fs::rename(
            repo.join(".pipeline/snapshots"),
            outside.path().join("snapshots"),
        )
        .unwrap();
        symlink(
            outside.path().join("snapshots"),
            repo.join(".pipeline/snapshots"),
        )
        .unwrap();
        assert!(protect_run(repo, id, "important", true).is_err());
        assert!(complete_run(repo, id).is_err());
        assert_eq!(
            fs::read(
                outside
                    .path()
                    .join("snapshots")
                    .join(id.to_string())
                    .join("retention.json")
            )
            .unwrap(),
            original
        );
    }
    #[test]
    fn lease_child() {
        let Some(repo) = std::env::var_os("SNAPSHOT_LEASE_TEST_REPO") else {
            return;
        };
        let _lease = StorageLease::inspection(Path::new(&repo)).unwrap();
        println!("LEASE_READY");
        std::io::stdout().flush().unwrap();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
    }
    #[test]
    fn cleanup_respects_a_reviewer_in_another_process() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        for _ in 0..3 {
            run(repo, 1, 10, true);
        }
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "snapshot_storage::tests::lease_child",
                "--exact",
                "--nocapture",
            ])
            .env("SNAPSHOT_LEASE_TEST_REPO", repo)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line.contains("LEASE_READY") {
                break;
            }
        }
        assert_eq!(
            retain(repo, &policy(3, 1), false).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        child.stdin.take().unwrap().write_all(b"release\n").unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(
            retain(repo, &policy(3, 1), false).unwrap().after.snapshots,
            1
        );
    }
    #[test]
    #[ignore = "130 MiB incompressible artifacts and full binary revision resource experiment"]
    fn incompressible_revision_resource_probe() {
        fn write_artifacts(repo: &Path, seed: u64) {
            for i in 0..65u64 {
                let mut state = seed.wrapping_add(i * 19 + 1);
                let mut bytes = vec![0u8; 2 << 20];
                for chunk in bytes.as_chunks_mut::<8>().0 {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    chunk.copy_from_slice(&state.to_le_bytes());
                }
                fs::write(repo.join(format!("artifact-{i:02}.bin")), bytes).unwrap();
            }
        }
        let tmp = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let repo = tmp.path();
        write_artifacts(repo, 10000);
        let id = uuid::Uuid::new_v4();
        assert!(
            ArtifactSnapshot::collect(repo, "probe", id, 0)
                .unwrap_err()
                .to_string()
                .contains("limit exceeded")
        );
        let limits = CaptureLimits {
            max_total_bytes: 512 << 20,
            ..Default::default()
        };
        let start = std::time::Instant::now();
        let baseline =
            ArtifactSnapshot::collect_configured(repo, "probe", id, 0, None, &limits).unwrap();
        write_artifacts(repo, 90000);
        let revised =
            ArtifactSnapshot::collect_configured(repo, "probe", id, 1, Some(&baseline), &limits)
                .unwrap();
        assert_eq!(
            revised.task_relative_changed_paths.as_ref().unwrap().len(),
            65
        );
        let patch_bytes = fs::metadata(revised.evidence_dir.join("task-relative.patch"))
            .unwrap()
            .len();
        assert!(patch_bytes > 130 << 20);
        println!(
            "130 MiB incompressible r0 + fully changed r1: {:?}; patch bytes={patch_bytes}; stored bytes={}",
            start.elapsed(),
            tree_bytes(&run_root(repo, id)).unwrap()
        );
    }
    #[test]
    #[ignore = "512 MiB capture resource experiment; synthetic temporary data only"]
    fn large_capture_resource_probe() {
        let tmp = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let repo = tmp.path();
        let block = vec![0x5a; 16 << 20];
        for i in 0..32 {
            fs::write(repo.join(format!("artifact-{i:02}.bin")), &block).unwrap();
        }
        drop(block);
        let id = uuid::Uuid::new_v4();
        assert!(
            ArtifactSnapshot::collect(repo, "probe", id, 0)
                .unwrap_err()
                .to_string()
                .contains("limit exceeded")
        );
        let limits = CaptureLimits {
            max_total_bytes: 512 << 20,
            ..Default::default()
        };
        let start = std::time::Instant::now();
        let baseline =
            ArtifactSnapshot::collect_configured(repo, "probe", id, 0, None, &limits).unwrap();
        assert_eq!(baseline.changed_files.len(), 32);
        let mut changed = vec![0x5a; 16 << 20];
        changed[0] = 0;
        fs::write(repo.join("artifact-00.bin"), changed).unwrap();
        let revised =
            ArtifactSnapshot::collect_configured(repo, "probe", id, 1, Some(&baseline), &limits)
                .unwrap();
        assert_eq!(
            revised.task_relative_changed_paths,
            Some(vec!["artifact-00.bin".into()])
        );
        println!(
            "512 MiB r0 + r1 with baseline verification and binary patch: {:?}; stored bytes={}",
            start.elapsed(),
            tree_bytes(&run_root(repo, id)).unwrap()
        );
    }
}
