use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

const MIGRATION_SCHEMA: &str = "llm-harness.runtime-app.data-migration.v1";
const REPORT_SCHEMA: &str = "llm-harness.runtime-app.data-migration-report.v1";

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("source data root must be an existing directory: {0}")]
    InvalidSource(PathBuf),
    #[error("destination data root must be an existing directory: {0}")]
    InvalidDestination(PathBuf),
    #[error("backup data root must be an existing directory: {0}")]
    InvalidBackup(PathBuf),
    #[error("destination data root already exists: {0}")]
    DestinationExists(PathBuf),
    #[error("backup path already exists: {0}")]
    BackupExists(PathBuf),
    #[error("source and destination must be different directories: {0}")]
    SamePath(PathBuf),
    #[error("destination cannot be inside the source data root: {0}")]
    DestinationInsideSource(PathBuf),
    #[error("source cannot be inside the destination data root: {0}")]
    SourceInsideDestination(PathBuf),
    #[error("backup cannot be inside the source or destination data root: {0}")]
    BackupInsideDataRoot(PathBuf),
    #[error("rollback paths must not be nested: {0}")]
    NestedRollbackPaths(PathBuf),
    #[error("data root contains an unsupported entry: {0}")]
    UnsupportedEntry(PathBuf),
    #[error("migration preparation failed before changing the source data root: {0}")]
    Preparation(#[source] io::Error),
    #[error("migration commit failed and was rolled back: {0}")]
    CommitRolledBack(#[source] io::Error),
    #[error(
        "migration rollback failed; source may be unavailable, backup is preserved at {backup}"
    )]
    RollbackFailed {
        backup: PathBuf,
        #[source]
        error: io::Error,
    },
    #[error("migration journal is invalid: {0}")]
    InvalidJournal(String),
    #[error("migration state is inconsistent; journal: {0}")]
    InconsistentJournal(PathBuf),
    #[error("migration completed but requires recovery; rerun the same command. Journal: {0}")]
    RecoveryRequired(PathBuf),
    #[error("migration completed but journal cleanup failed. Journal: {0}")]
    JournalCleanup(PathBuf),
    #[error("migration report write failed: {0}")]
    Report(#[source] io::Error),
    #[error("usage: {0}")]
    Usage(String),
}

#[derive(Debug, Clone)]
pub enum DataCommand {
    Migrate {
        source: PathBuf,
        destination: PathBuf,
        backup: Option<PathBuf>,
    },
    Rollback {
        source: PathBuf,
        destination: PathBuf,
        backup: PathBuf,
    },
}

#[derive(Debug, Serialize)]
pub struct MigrationResult {
    pub operation: &'static str,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub backup: PathBuf,
    pub report_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MigrationJournal {
    schema: String,
    operation: MigrationOperation,
    phase: MigrationPhase,
    source: PathBuf,
    destination: PathBuf,
    backup: PathBuf,
    staging: Option<PathBuf>,
    preserved: Option<PathBuf>,
    started_at: String,
    updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MigrationOperation {
    Migrate,
    Rollback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MigrationPhase {
    Prepared,
    SourceRenamed,
    DestinationRenamed,
    Completed,
    RolledBack,
}

#[derive(Debug, Default)]
struct CopyStats {
    files: u64,
    directories: u64,
    bytes: u64,
}

struct PathGuard {
    _file: File,
}

#[cfg(windows)]
const WINDOWS_FILE_SHARE_DELETE: u32 = 0x0000_0001;

pub fn parse_data_command(arguments: &[String]) -> Result<Option<DataCommand>, MigrationError> {
    let Some(command_name) = arguments.first() else {
        return Ok(None);
    };
    match command_name.as_str() {
        "migrate-data" => {
            let source = required_flag(arguments, "--from")?;
            let destination = required_flag(arguments, "--to")?;
            let backup = optional_flag(arguments, "--backup")?;
            reject_remaining_arguments(arguments, &["--from", "--to", "--backup"])?;
            Ok(Some(DataCommand::Migrate {
                source: source.into(),
                destination: destination.into(),
                backup: backup.map(PathBuf::from),
            }))
        }
        "rollback-data" => {
            let source = required_flag(arguments, "--from")?;
            let destination = required_flag(arguments, "--to")?;
            let backup = required_flag(arguments, "--backup")?;
            reject_remaining_arguments(arguments, &["--from", "--to", "--backup"])?;
            Ok(Some(DataCommand::Rollback {
                source: source.into(),
                destination: destination.into(),
                backup: backup.into(),
            }))
        }
        _ => Ok(None),
    }
}

pub fn run_data_command(command: DataCommand) -> Result<MigrationResult, MigrationError> {
    match command {
        DataCommand::Migrate {
            source,
            destination,
            backup,
        } => migrate_data_root(source, destination, backup),
        DataCommand::Rollback {
            source,
            destination,
            backup,
        } => rollback_data_root(source, destination, backup),
    }
}

pub fn migrate_data_root(
    source: PathBuf,
    destination: PathBuf,
    requested_backup: Option<PathBuf>,
) -> Result<MigrationResult, MigrationError> {
    let source = resolve_directory_or_target(source, MigrationError::InvalidSource)?;
    let destination = normalize_target(&destination)?;
    validate_migration_paths(&source, &destination)?;

    let journal_path = journal_path(&source, &destination);
    let _journal_lock = lock_journal(&journal_path)?;
    let _destination_lock = lock_destination(&destination)?;

    if journal_path.exists() {
        let journal = read_journal(&journal_path)?;
        validate_journal(&journal, MigrationOperation::Migrate, &source, &destination)?;
        if let Some(backup) = &requested_backup {
            let requested_backup = normalize_target(backup)?;
            if requested_backup != journal.backup {
                return Err(MigrationError::InvalidJournal(format!(
                    "backup must match the pending journal backup {}",
                    journal.backup.display()
                )));
            }
        }
        let _recovery_source_lock = if path_exists(&journal.source) {
            Some(lock_data_root(&journal.source)?)
        } else if path_exists(&journal.backup) {
            Some(lock_data_root(&journal.backup)?)
        } else {
            None
        };
        let _recovery_destination_lock =
            if journal.phase != MigrationPhase::Completed && path_exists(&journal.destination) {
                Some(lock_data_root(&journal.destination)?)
            } else {
                None
            };
        return recover_migrate(journal, journal_path);
    }

    let _source_lock = lock_data_root(&source)?;

    let backup = match requested_backup {
        Some(backup) => normalize_target(&backup)?,
        None => unique_sibling(&source, "llm-harness-app-migration-backup"),
    };
    if path_exists(&backup) {
        return Err(MigrationError::BackupExists(backup));
    }
    validate_backup_path(&source, &destination, &backup)?;
    let staging = unique_sibling(&destination, "llm-harness-app-migration-staging");
    if path_exists(&staging) || path_exists(&destination) {
        return Err(MigrationError::DestinationExists(destination));
    }

    let started_at = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
    let mut stats = CopyStats::default();
    copy_data_root(&source, &staging, &mut stats).map_err(|error| {
        remove_path(&staging);
        MigrationError::Preparation(error)
    })?;
    let report_path = staging.join("migration-report.json");
    let report = migration_report(&source, &destination, &backup, &stats, &started_at);
    write_report(&report_path, &report).map_err(|error| {
        remove_path(&staging);
        MigrationError::Report(error)
    })?;

    let journal = MigrationJournal {
        schema: MIGRATION_SCHEMA.to_owned(),
        operation: MigrationOperation::Migrate,
        phase: MigrationPhase::Prepared,
        source: source.clone(),
        destination: destination.clone(),
        backup: backup.clone(),
        staging: Some(staging.clone()),
        preserved: None,
        started_at: started_at.clone(),
        updated_at: started_at,
    };
    if let Err(error) = write_journal(&journal_path, &journal) {
        remove_path(&staging);
        return Err(error);
    }
    commit_migrate(journal, journal_path)
}

pub fn rollback_data_root(
    source: PathBuf,
    destination: PathBuf,
    backup: PathBuf,
) -> Result<MigrationResult, MigrationError> {
    let destination = resolve_directory_or_target(destination, MigrationError::InvalidDestination)?;
    let source = normalize_target(&source)?;
    let backup = resolve_directory_or_target(backup, MigrationError::InvalidBackup)?;

    let journal_path = journal_path(&source, &destination);
    let _journal_lock = lock_journal(&journal_path)?;

    if journal_path.exists() {
        let journal = read_journal(&journal_path)?;
        validate_journal(
            &journal,
            MigrationOperation::Rollback,
            &source,
            &destination,
        )?;
        if journal.backup != backup {
            return Err(MigrationError::InvalidJournal(format!(
                "backup must match the pending journal backup {}",
                journal.backup.display()
            )));
        }
        let _recovery_source_lock = if path_exists(&journal.source) {
            Some(lock_data_root(&journal.source)?)
        } else {
            None
        };
        let _recovery_destination_lock = if path_exists(&journal.destination) {
            Some(lock_data_root(&journal.destination)?)
        } else {
            None
        };
        let _recovery_backup_lock = if path_exists(&journal.backup) {
            Some(lock_data_root(&journal.backup)?)
        } else {
            None
        };
        return recover_rollback(journal, journal_path);
    }

    if path_exists(&source) {
        return Err(MigrationError::DestinationExists(source));
    }
    validate_rollback_paths(&source, &destination, &backup)?;
    let _destination_lock = lock_data_root(&destination)?;
    let _backup_lock = lock_data_root(&backup)?;

    let preserved = unique_sibling(&destination, "llm-harness-app-rollback-preserved");
    if path_exists(&preserved) {
        return Err(MigrationError::DestinationExists(preserved));
    }
    let started_at = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
    let journal = MigrationJournal {
        schema: MIGRATION_SCHEMA.to_owned(),
        operation: MigrationOperation::Rollback,
        phase: MigrationPhase::Prepared,
        source: source.clone(),
        destination: destination.clone(),
        backup: backup.clone(),
        staging: None,
        preserved: Some(preserved.clone()),
        started_at: started_at.clone(),
        updated_at: started_at.clone(),
    };
    write_journal(&journal_path, &journal)?;

    commit_rollback(journal, journal_path)
}

fn commit_migrate(
    mut journal: MigrationJournal,
    journal_path: PathBuf,
) -> Result<MigrationResult, MigrationError> {
    let staging = journal.staging.clone().ok_or_else(|| {
        MigrationError::InvalidJournal("migrate journal has no staging path".into())
    })?;
    let migration_completed_consistently = !path_exists(&journal.source)
        && path_exists(&journal.destination)
        && path_exists(&journal.backup)
        && !path_exists(&staging);
    if journal.phase == MigrationPhase::Completed && !migration_completed_consistently {
        return Err(MigrationError::InconsistentJournal(journal_path));
    }
    if journal.phase == MigrationPhase::Prepared {
        if path_exists(&journal.source)
            && !path_exists(&journal.backup)
            && !path_exists(&journal.destination)
        {
            rename_durable(&journal.source, &journal.backup)
                .map_err(|error| cleanup_prepared_migration(&staging, &journal_path, error))?;
        } else if !path_exists(&journal.source)
            && path_exists(&journal.backup)
            && !path_exists(&journal.destination)
        {
        } else {
            return Err(MigrationError::InconsistentJournal(journal_path));
        }
        journal.phase = MigrationPhase::SourceRenamed;
        journal.updated_at = timestamp();
        write_journal(&journal_path, &journal)?;
    }
    if journal.phase == MigrationPhase::SourceRenamed {
        if !path_exists(&journal.destination) && path_exists(&staging) {
            if let Err(error) = rename_durable(&staging, &journal.destination) {
                return Err(rollback_committed_migration(
                    &journal,
                    &journal_path,
                    &staging,
                    error,
                ));
            }
        } else if path_exists(&journal.destination) && !path_exists(&staging) {
        } else {
            return Err(MigrationError::InconsistentJournal(journal_path));
        }
        journal.phase = MigrationPhase::Completed;
        journal.updated_at = timestamp();
        write_journal(&journal_path, &journal)
            .map_err(|_| MigrationError::RecoveryRequired(journal_path.clone()))?;
    }
    finish_journal(&journal_path)?;
    Ok(MigrationResult {
        operation: "migrate",
        source: journal.source,
        destination: journal.destination.clone(),
        backup: journal.backup,
        report_path: journal.destination.join("migration-report.json"),
    })
}

fn recover_migrate(
    journal: MigrationJournal,
    journal_path: PathBuf,
) -> Result<MigrationResult, MigrationError> {
    match journal.phase {
        MigrationPhase::Prepared | MigrationPhase::SourceRenamed | MigrationPhase::Completed => {
            commit_migrate(journal, journal_path)
        }
        MigrationPhase::DestinationRenamed => {
            Err(MigrationError::InconsistentJournal(journal_path))
        }
        MigrationPhase::RolledBack => recover_rolled_back_migration(journal, journal_path),
    }
}

fn recover_rolled_back_migration(
    journal: MigrationJournal,
    journal_path: PathBuf,
) -> Result<MigrationResult, MigrationError> {
    let staging = journal.staging.clone().ok_or_else(|| {
        MigrationError::InvalidJournal("migrate journal has no staging path".into())
    })?;
    if !path_exists(&journal.source)
        || path_exists(&journal.backup)
        || path_exists(&journal.destination)
    {
        return Err(MigrationError::InconsistentJournal(journal_path));
    }
    if path_exists(&staging) {
        std::fs::remove_dir_all(&staging)
            .map_err(|_| MigrationError::RecoveryRequired(journal_path.clone()))?;
    }
    finish_journal(&journal_path)?;
    Err(MigrationError::CommitRolledBack(io::Error::other(
        "migration commit was rolled back; rerun the command to retry",
    )))
}

fn commit_rollback(
    mut journal: MigrationJournal,
    journal_path: PathBuf,
) -> Result<MigrationResult, MigrationError> {
    let preserved = journal.preserved.clone().ok_or_else(|| {
        MigrationError::InvalidJournal("rollback journal has no preserved path".into())
    })?;
    let report_path = preserved.join("migration-rollback-report.json");
    let rollback_completed_consistently = path_exists(&journal.source)
        && !path_exists(&journal.destination)
        && !path_exists(&journal.backup)
        && path_exists(&preserved);
    if journal.phase == MigrationPhase::Completed && !rollback_completed_consistently {
        return Err(MigrationError::InconsistentJournal(journal_path));
    }
    if journal.phase == MigrationPhase::Prepared
        || journal.phase == MigrationPhase::DestinationRenamed
    {
        let rollback_not_started = path_exists(&journal.destination)
            && path_exists(&journal.backup)
            && !path_exists(&journal.source)
            && !path_exists(&preserved);
        let mut destination_is_preserved = !path_exists(&journal.destination)
            && path_exists(&preserved)
            && !path_exists(&journal.source);
        let source_is_restored = path_exists(&journal.source)
            && !path_exists(&journal.backup)
            && path_exists(&preserved);
        if rollback_not_started && journal.phase == MigrationPhase::Prepared {
            rename_durable(&journal.destination, &preserved)
                .map_err(|_| MigrationError::RecoveryRequired(journal_path.clone()))?;
            destination_is_preserved = true;
        } else if source_is_restored || destination_is_preserved {
        } else {
            return Err(MigrationError::InconsistentJournal(journal_path));
        }
        if destination_is_preserved {
            rename_durable(&journal.backup, &journal.source).map_err(|error| {
                restore_rollback_destination(&journal.destination, &preserved, &journal_path, error)
            })?;
        }
        if !path_exists(&report_path) {
            let report = rollback_report(
                &journal.source,
                &journal.destination,
                &journal.backup,
                &preserved,
                &journal.started_at,
            );
            if let Err(error) = write_report(&report_path, &report) {
                if destination_is_preserved {
                    return Err(restore_rollback_destination(
                        &journal.destination,
                        &preserved,
                        &journal_path,
                        error,
                    ));
                }
                return Err(MigrationError::RecoveryRequired(journal_path));
            }
        }
        journal.phase = MigrationPhase::Completed;
        journal.updated_at = timestamp();
        write_journal(&journal_path, &journal)
            .map_err(|_| MigrationError::RecoveryRequired(journal_path.clone()))?;
    }
    finish_journal(&journal_path)?;
    Ok(MigrationResult {
        operation: "rollback",
        source: journal.source,
        destination: journal.destination,
        backup: journal.backup,
        report_path,
    })
}

fn recover_rollback(
    journal: MigrationJournal,
    journal_path: PathBuf,
) -> Result<MigrationResult, MigrationError> {
    commit_rollback(journal, journal_path)
}

fn rollback_committed_migration(
    journal: &MigrationJournal,
    journal_path: &Path,
    staging: &Path,
    error: io::Error,
) -> MigrationError {
    let restore_result = rename_durable(&journal.backup, &journal.source);
    if let Err(restore_error) = restore_result {
        return MigrationError::RollbackFailed {
            backup: journal.backup.clone(),
            error: restore_error,
        };
    }

    let mut rolled_back_journal = journal.clone();
    rolled_back_journal.phase = MigrationPhase::RolledBack;
    rolled_back_journal.updated_at = timestamp();
    if write_journal(journal_path, &rolled_back_journal).is_err() {
        if path_exists(staging) {
            let _ = std::fs::remove_dir_all(staging);
        }
        let _ = std::fs::remove_file(journal_path);
        return MigrationError::JournalCleanup(journal_path.to_path_buf());
    }

    if path_exists(staging)
        && let Err(cleanup_error) = std::fs::remove_dir_all(staging)
    {
        return MigrationError::CommitRolledBack(cleanup_error);
    }
    if let Err(cleanup_error) = finish_journal(journal_path) {
        let _ = cleanup_error;
        return MigrationError::JournalCleanup(journal_path.to_path_buf());
    }
    MigrationError::CommitRolledBack(error)
}

fn cleanup_prepared_migration(
    staging: &Path,
    journal_path: &Path,
    error: io::Error,
) -> MigrationError {
    if staging.exists() {
        let _ = std::fs::remove_dir_all(staging);
    }
    let _ = std::fs::remove_file(journal_path);
    MigrationError::CommitRolledBack(error)
}

fn restore_rollback_destination(
    destination: &Path,
    preserved: &Path,
    journal_path: &Path,
    error: io::Error,
) -> MigrationError {
    if !path_exists(destination)
        && path_exists(preserved)
        && let Err(restore_error) = rename_durable(preserved, destination)
    {
        return MigrationError::RollbackFailed {
            backup: preserved.to_path_buf(),
            error: restore_error,
        };
    }
    let _ = std::fs::remove_file(journal_path);
    MigrationError::CommitRolledBack(error)
}

fn path_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn copy_tree(source: &Path, destination: &Path, stats: &mut CopyStats) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    if metadata.is_dir() {
        stats.directories += 1;
        std::fs::create_dir_all(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &destination.join(entry.file_name()), stats)?;
        }
        set_unix_permissions(destination, &metadata)?;
        sync_directory(destination)?;
    } else if metadata.is_file() {
        stats.files += 1;
        stats.bytes += metadata.len();
        std::fs::copy(source, destination)?;
        sync_file(destination)?;
    } else {
        return Err(io::Error::other(format!(
            "unsupported non-regular entry: {}",
            source.display()
        )));
    }
    Ok(())
}

fn copy_data_root(source: &Path, destination: &Path, stats: &mut CopyStats) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    if !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "not a directory: {}",
            source.display()
        )));
    }
    stats.directories += 1;
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_name() == std::ffi::OsStr::new("studio.lock") {
            continue;
        }
        copy_tree(&entry.path(), &destination.join(entry.file_name()), stats)?;
    }
    set_unix_permissions(destination, &metadata).and_then(|()| sync_directory(destination))
}

fn remove_path(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            let _ = std::fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = std::fs::remove_file(path);
        }
        Err(_) => {}
    }
}

fn rename_durable(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)?;
    if let Some(parent) = source.parent() {
        sync_directory(parent)?;
    }
    if let Some(parent) = destination.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Flush a regular file to stable storage.
///
/// `FlushFileBuffers` on Windows requires a handle with write access;
/// `File::open` produces a read-only handle and fails with
/// `ERROR_ACCESS_DENIED`. Reopen for writing so the durability call works on
/// every platform. A read-only destination cannot be reopened for writing, so
/// its flush is best-effort (the data was already written by the copy).
fn sync_file(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        match std::fs::OpenOptions::new().write(true).open(path) {
            Ok(file) => file.sync_all(),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(()),
            Err(error) => Err(error),
        }
    }
    #[cfg(not(windows))]
    {
        File::open(path)?.sync_all()
    }
}

#[cfg(unix)]
fn set_unix_permissions(path: &Path, metadata: &std::fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        path,
        std::fs::Permissions::from_mode(metadata.permissions().mode()),
    )
}

#[cfg(not(unix))]
fn set_unix_permissions(_path: &Path, _metadata: &std::fs::Metadata) -> io::Result<()> {
    Ok(())
}

fn migration_report(
    source: &Path,
    destination: &Path,
    backup: &Path,
    stats: &CopyStats,
    started_at: &str,
) -> serde_json::Value {
    serde_json::json!({
        "schema": REPORT_SCHEMA,
        "operation": "migrate",
        "status": "completed",
        "source": source,
        "destination": destination,
        "backup": backup,
        "files": stats.files,
        "directories": stats.directories,
        "bytes": stats.bytes,
        "started_at": started_at,
        "completed_at": timestamp(),
    })
}

fn rollback_report(
    source: &Path,
    destination: &Path,
    backup: &Path,
    preserved: &Path,
    started_at: &str,
) -> serde_json::Value {
    serde_json::json!({
        "schema": REPORT_SCHEMA,
        "operation": "rollback",
        "status": "completed",
        "restored_source": source,
        "former_destination": destination,
        "restored_from_backup": backup,
        "preserved_former_destination": preserved,
        "started_at": started_at,
        "completed_at": timestamp(),
    })
}

fn write_report(path: &Path, report: &serde_json::Value) -> io::Result<()> {
    write_private_json(path, report)
}

fn write_journal(path: &Path, journal: &MigrationJournal) -> Result<(), MigrationError> {
    write_private_json(path, journal)
        .map_err(|error| MigrationError::InvalidJournal(format!("write failed: {error}")))
}

fn write_private_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    let mut contents = serde_json::to_vec_pretty(value)?;
    contents.push(b'\n');
    let write_result = (|| {
        let mut file = File::create(&temporary)?;
        file.write_all(&contents)?;
        file.sync_all()
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
    }
    sync_file(&temporary)?;
    rename_durable(&temporary, path)
}

fn read_journal(path: &Path) -> Result<MigrationJournal, MigrationError> {
    let contents = std::fs::read_to_string(path)
        .map_err(|error| MigrationError::InvalidJournal(format!("read failed: {error}")))?;
    serde_json::from_str(&contents)
        .map_err(|error| MigrationError::InvalidJournal(format!("parse failed: {error}")))
}

fn validate_journal(
    journal: &MigrationJournal,
    operation: MigrationOperation,
    source: &Path,
    destination: &Path,
) -> Result<(), MigrationError> {
    if journal.schema != MIGRATION_SCHEMA
        || journal.operation != operation
        || journal.source != source
        || journal.destination != destination
    {
        return Err(MigrationError::InvalidJournal(
            "journal does not match the requested operation and paths".into(),
        ));
    }
    Ok(())
}

fn finish_journal(journal_path: &Path) -> Result<(), MigrationError> {
    std::fs::remove_file(journal_path)
        .and_then(|()| journal_path.parent().map_or(Ok(()), sync_directory))
        .map_err(|_| MigrationError::JournalCleanup(journal_path.to_path_buf()))
}

fn canonicalize_existing_directory(path: &Path) -> io::Result<PathBuf> {
    let path = path.canonicalize()?;
    if !path.is_dir() {
        return Err(io::Error::other(format!(
            "not a directory: {}",
            path.display()
        )));
    }
    Ok(path)
}

fn normalize_target(path: &Path) -> Result<PathBuf, MigrationError> {
    let file_name = path
        .file_name()
        .filter(|name| {
            !name.is_empty()
                && *name != std::ffi::OsStr::new(".")
                && *name != std::ffi::OsStr::new("..")
        })
        .ok_or_else(|| MigrationError::Usage(format!("invalid target path: {}", path.display())))?;
    let parent = path.parent().ok_or_else(|| {
        MigrationError::Usage(format!("target path has no parent: {}", path.display()))
    })?;
    std::fs::create_dir_all(parent).map_err(MigrationError::Preparation)?;
    let parent = parent.canonicalize().map_err(MigrationError::Preparation)?;
    Ok(parent.join(file_name))
}

fn resolve_directory_or_target(
    path: PathBuf,
    invalid: fn(PathBuf) -> MigrationError,
) -> Result<PathBuf, MigrationError> {
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() => {
            let canonical =
                canonicalize_existing_directory(&path).map_err(|_| invalid(path.clone()))?;
            if canonical.parent().is_none() {
                return Err(invalid(canonical));
            }
            Ok(canonical)
        }
        Ok(_) => Err(invalid(path)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => normalize_target(&path),
        Err(_) => Err(invalid(path)),
    }
}

fn validate_migration_paths(source: &Path, destination: &Path) -> Result<(), MigrationError> {
    if source == destination {
        return Err(MigrationError::SamePath(source.to_path_buf()));
    }
    if destination.starts_with(source) {
        return Err(MigrationError::DestinationInsideSource(
            destination.to_path_buf(),
        ));
    }
    if source.starts_with(destination) {
        return Err(MigrationError::SourceInsideDestination(
            source.to_path_buf(),
        ));
    }
    Ok(())
}

fn validate_backup_path(
    source: &Path,
    destination: &Path,
    backup: &Path,
) -> Result<(), MigrationError> {
    if backup.starts_with(source) || backup.starts_with(destination) {
        return Err(MigrationError::BackupInsideDataRoot(backup.to_path_buf()));
    }
    Ok(())
}

fn validate_rollback_paths(
    source: &Path,
    destination: &Path,
    backup: &Path,
) -> Result<(), MigrationError> {
    if source == destination || source == backup || destination == backup {
        return Err(MigrationError::SamePath(source.to_path_buf()));
    }
    if source.starts_with(destination)
        || source.starts_with(backup)
        || destination.starts_with(source)
        || destination.starts_with(backup)
        || backup.starts_with(source)
        || backup.starts_with(destination)
    {
        return Err(MigrationError::NestedRollbackPaths(source.to_path_buf()));
    }
    Ok(())
}

fn unique_sibling(path: &Path, prefix: &str) -> PathBuf {
    let parent = path.parent().expect("path has a parent");
    parent.join(format!("{prefix}-{}", uuid::Uuid::new_v4()))
}

fn journal_path(source: &Path, destination: &Path) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(source.as_os_str().as_encoded_bytes());
    hasher.update([0]);
    hasher.update(destination.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    let name = format!("llm-harness-app-migration-{}.json", hex(&digest));
    source.parent().unwrap_or_else(|| Path::new(".")).join(name)
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn lock_journal(path: &Path) -> Result<PathGuard, MigrationError> {
    let lock_path = path.with_extension("json.lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(MigrationError::Preparation)?;
    }
    let file = open_lock_file(&lock_path, false).map_err(MigrationError::Preparation)?;
    Ok(PathGuard { _file: file })
}

fn lock_data_root(path: &Path) -> Result<PathGuard, MigrationError> {
    let lock_path = path.join("studio.lock");
    let file = open_lock_file(&lock_path, true).map_err(MigrationError::Preparation)?;
    Ok(PathGuard { _file: file })
}

fn lock_destination(path: &Path) -> Result<PathGuard, MigrationError> {
    let lock_path = path.with_extension("migration.lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(MigrationError::Preparation)?;
    }
    let file = open_lock_file(&lock_path, false).map_err(MigrationError::Preparation)?;
    Ok(PathGuard { _file: file })
}

fn open_lock_file(path: &Path, _share_delete: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(if _share_delete {
            WINDOWS_FILE_SHARE_DELETE
        } else {
            0
        });
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(not(unix))]
    let _ = &file;
    Ok(file)
}

fn required_flag<'a>(arguments: &'a [String], flag: &str) -> Result<&'a str, MigrationError> {
    optional_flag(arguments, flag)?.ok_or_else(|| MigrationError::Usage(format!("missing {flag}")))
}

fn optional_flag<'a>(
    arguments: &'a [String],
    flag: &str,
) -> Result<Option<&'a str>, MigrationError> {
    let mut values = arguments
        .iter()
        .skip(1)
        .skip_while(|argument| argument.as_str() != flag);
    values.next();
    match values.next() {
        Some(value) if !value.starts_with("--") => Ok(Some(value)),
        Some(_) => Err(MigrationError::Usage(format!("{flag} requires a value"))),
        None => Ok(None),
    }
}

fn reject_remaining_arguments(
    arguments: &[String],
    known_flags: &[&str],
) -> Result<(), MigrationError> {
    for flag in known_flags {
        let count = arguments
            .iter()
            .skip(1)
            .filter(|argument| argument.as_str() == *flag)
            .count();
        if count > 1 {
            return Err(MigrationError::Usage(format!("duplicate flag: {flag}")));
        }
    }
    let mut index = 1;
    while index < arguments.len() {
        if !known_flags.contains(&arguments[index].as_str()) {
            return Err(MigrationError::Usage(format!(
                "unexpected argument: {}",
                arguments[index]
            )));
        }
        index += 2;
    }
    Ok(())
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_agent_team_studio::settings::Settings;
    use llm_harness_agent_team_studio::team_manager::list_persisted;
    use std::fs;
    use std::time::SystemTime;

    fn temp_root(test: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "runtime-app-migration-{test}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn test_case(test: &str) -> PathBuf {
        let root = temp_root(test);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn source_with_data() -> PathBuf {
        let root = test_case("source").join("source");
        fs::create_dir_all(root.join("teams/demo")).unwrap();
        fs::write(
            root.join("settings.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "models": {"strong": "strong-model", "main": "main-model", "cheap": "cheap-model"},
                "scout_interval_secs": 17,
                "base_url": "http://localhost/v1"
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(root.join("apikey"), "secret-key").unwrap();
        fs::write(
            root.join("teams/demo/team.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "id": "demo", "name": "Demo", "members": []
            }))
            .unwrap(),
        )
        .unwrap();
        fs::create_dir_all(root.join("logs")).unwrap();
        fs::write(root.join("logs/runtime.jsonl"), "{\"event\":\"startup\"}\n").unwrap();
        root
    }

    #[test]
    fn copied_files_are_synced_with_a_writable_handle() {
        let root = test_case("copy-sync");
        let source = root.join("source.txt");
        let destination = root.join("destination.txt");
        fs::write(&source, b"payload").unwrap();
        let mut stats = CopyStats::default();
        copy_tree(&source, &destination, &mut stats).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"payload");
        assert_eq!(stats.files, 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parse_data_commands_accepts_only_supported_flags() {
        let arguments = [
            "migrate-data".to_owned(),
            "--from".to_owned(),
            "/source".to_owned(),
            "--to".to_owned(),
            "/destination".to_owned(),
            "--backup".to_owned(),
            "/backup".to_owned(),
        ];
        assert!(matches!(
            parse_data_command(&arguments).unwrap(),
            Some(DataCommand::Migrate { .. })
        ));

        let arguments = [
            "rollback-data".to_owned(),
            "--from".to_owned(),
            "/source".to_owned(),
            "--to".to_owned(),
            "/destination".to_owned(),
            "--backup".to_owned(),
            "/backup".to_owned(),
        ];
        assert!(matches!(
            parse_data_command(&arguments).unwrap(),
            Some(DataCommand::Rollback { .. })
        ));

        let arguments = ["migrate-data".to_owned(), "--unexpected".to_owned()];
        assert!(parse_data_command(&arguments).is_err());

        let arguments = [
            "migrate-data".to_owned(),
            "--from".to_owned(),
            "/source".to_owned(),
            "--to".to_owned(),
            "/destination".to_owned(),
            "--from".to_owned(),
            "/other".to_owned(),
        ];
        let error = parse_data_command(&arguments).unwrap_err();
        assert!(error.to_string().contains("duplicate flag"));
    }

    #[test]
    fn migration_preserves_settings_teams_and_creates_report_and_backup() {
        let source = source_with_data();
        let parent = source.parent().unwrap().to_path_buf();
        let destination = parent.join("migrated-root");
        let backup = parent.join("migration-backup");

        let result =
            migrate_data_root(source.clone(), destination.clone(), Some(backup.clone())).unwrap();

        assert!(!source.exists());
        assert!(destination.join("settings.json").exists());
        assert!(!destination.join("studio.lock").exists());
        assert_eq!(
            fs::read_to_string(destination.join("apikey")).unwrap(),
            "secret-key"
        );
        assert_eq!(list_persisted(&destination).len(), 1);
        assert_eq!(
            Settings::load(&destination)
                .unwrap()
                .unwrap()
                .scout_interval_secs,
            17
        );
        assert!(backup.join("teams/demo/team.json").exists());
        assert_eq!(
            result.report_path,
            destination.join("migration-report.json")
        );
        let report: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(result.report_path).unwrap()).unwrap();
        assert_eq!(report["schema"], REPORT_SCHEMA);
        assert_eq!(report["operation"], "migrate");
        assert_eq!(report["status"], "completed");
        assert_eq!(report["files"], 4);

        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn migration_report_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let source = source_with_data();
        let parent = source.parent().unwrap().to_path_buf();
        let destination = parent.join("migrated-root");
        let backup = parent.join("migration-backup");
        let result = migrate_data_root(source, destination, Some(backup)).unwrap();
        let mode = fs::metadata(result.report_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn rollback_restores_source_and_preserves_former_destination() {
        let source = source_with_data();
        let parent = source.parent().unwrap().to_path_buf();
        let destination = parent.join("migrated-root");
        let backup = parent.join("migration-backup");
        migrate_data_root(source.clone(), destination.clone(), Some(backup.clone())).unwrap();
        fs::write(destination.join("changed-after-migration"), "new data").unwrap();

        let result =
            rollback_data_root(source.clone(), destination.clone(), backup.clone()).unwrap();

        assert!(source.join("settings.json").exists());
        assert_eq!(list_persisted(&source).len(), 1);
        assert!(!destination.exists());
        assert!(!backup.exists());
        let preserved = result.report_path.parent().unwrap();
        assert_eq!(
            fs::read_to_string(preserved.join("changed-after-migration")).unwrap(),
            "new data"
        );
        let report: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(result.report_path).unwrap()).unwrap();
        assert_eq!(report["operation"], "rollback");
        assert_eq!(report["status"], "completed");

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn existing_destination_is_rejected_without_changing_source() {
        let source = source_with_data();
        let parent = source.parent().unwrap().to_path_buf();
        let destination = parent.join("existing-destination");
        fs::create_dir_all(&destination).unwrap();

        let error = migrate_data_root(
            source.clone(),
            destination.clone(),
            Some(parent.join("backup")),
        )
        .unwrap_err();

        assert!(matches!(error, MigrationError::DestinationExists(_)));
        assert!(source.join("settings.json").exists());
        assert_eq!(list_persisted(&source).len(), 1);

        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn dangling_destination_symlink_is_rejected_as_existing() {
        use std::os::unix::fs::symlink;

        let source = source_with_data();
        let parent = source.parent().unwrap().to_path_buf();
        let destination = parent.join("migrated-root");
        symlink("missing", &destination).unwrap();

        let error = migrate_data_root(
            source.clone(),
            destination.clone(),
            Some(parent.join("backup")),
        )
        .unwrap_err();

        assert!(matches!(error, MigrationError::DestinationExists(_)));
        assert!(source.join("settings.json").exists());
        assert!(!parent.read_dir().unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("llm-harness-app-migration-staging")
        }));

        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn source_symlink_is_rejected_without_following_it() {
        use std::os::unix::fs::symlink;

        let parent = test_case("source-symlink");
        let target = parent.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("settings.json"), "{}").unwrap();
        let source = parent.join("source");
        symlink(&target, &source).unwrap();

        let error = migrate_data_root(
            source.clone(),
            parent.join("destination"),
            Some(parent.join("backup")),
        )
        .unwrap_err();

        assert!(matches!(error, MigrationError::InvalidSource(_)));
        assert!(target.join("settings.json").exists());

        fs::remove_dir_all(parent).unwrap();
    }

    // A symlink is the portable way to construct a non-regular entry on Unix.
    // Windows symlink creation needs Developer Mode or elevation, so the test
    // would otherwise "pass" by never creating the unsupported entry.
    #[cfg(unix)]
    #[test]
    fn unsupported_entry_fails_before_source_is_changed() {
        let source = source_with_data();
        let parent = source.parent().unwrap().to_path_buf();
        let destination = parent.join("migrated-root");
        std::os::unix::fs::symlink("settings.json", source.join("symlink")).unwrap();

        let error = migrate_data_root(
            source.clone(),
            destination.clone(),
            Some(parent.join("backup")),
        )
        .unwrap_err();

        assert!(matches!(error, MigrationError::Preparation(_)));
        assert!(source.join("settings.json").exists());
        assert!(!destination.exists());
        assert!(!parent.read_dir().unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("llm-harness-app-migration-staging")
        }));

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn prepared_journal_can_be_recovered_after_interruption() {
        let source = source_with_data();
        let parent = source.parent().unwrap().to_path_buf();
        let destination = parent.join("migrated-root");
        let backup = parent.join("migration-backup");
        let staging = parent.join("llm-harness-app-migration-staging-test");
        let mut stats = CopyStats::default();
        copy_data_root(&source, &staging, &mut stats).unwrap();
        let started_at = timestamp();
        let report = migration_report(&source, &destination, &backup, &stats, &started_at);
        write_report(&staging.join("migration-report.json"), &report).unwrap();
        let journal = MigrationJournal {
            schema: MIGRATION_SCHEMA.to_owned(),
            operation: MigrationOperation::Migrate,
            phase: MigrationPhase::Prepared,
            source: source.clone(),
            destination: destination.clone(),
            backup: backup.clone(),
            staging: Some(staging.clone()),
            preserved: None,
            started_at: started_at.clone(),
            updated_at: started_at,
        };
        let journal_path = journal_path(&source, &destination);
        write_journal(&journal_path, &journal).unwrap();

        let result =
            migrate_data_root(source.clone(), destination.clone(), Some(backup.clone())).unwrap();

        assert!(!source.exists());
        assert!(destination.join("settings.json").exists());
        assert!(backup.join("settings.json").exists());
        assert!(!journal_path.exists());
        assert_eq!(result.backup, backup);

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn source_renamed_journal_can_be_recovered_after_interruption() {
        let source = source_with_data();
        let parent = source.parent().unwrap().to_path_buf();
        let destination = parent.join("migrated-root");
        let backup = parent.join("migration-backup");
        let staging = parent.join("llm-harness-app-migration-staging-test");
        let mut stats = CopyStats::default();
        copy_data_root(&source, &staging, &mut stats).unwrap();
        fs::rename(&source, &backup).unwrap();
        let started_at = timestamp();
        let journal = MigrationJournal {
            schema: MIGRATION_SCHEMA.to_owned(),
            operation: MigrationOperation::Migrate,
            phase: MigrationPhase::Prepared,
            source: source.clone(),
            destination: destination.clone(),
            backup: backup.clone(),
            staging: Some(staging),
            preserved: None,
            started_at: started_at.clone(),
            updated_at: started_at,
        };
        let journal_path = journal_path(&source, &destination);
        write_journal(&journal_path, &journal).unwrap();

        migrate_data_root(source.clone(), destination.clone(), Some(backup.clone())).unwrap();

        assert!(!source.exists());
        assert!(destination.join("settings.json").exists());
        assert!(backup.join("settings.json").exists());
        assert!(!journal_path.exists());

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn failed_commit_rollback_preserves_the_recovery_journal() {
        let source = source_with_data();
        let parent = source.parent().unwrap().to_path_buf();
        let destination = parent.join("migrated-root");
        let backup = parent.join("missing-backup");
        let staging = parent.join("migration-staging");
        fs::create_dir_all(&staging).unwrap();
        fs::write(source.join("commit-marker"), "source").unwrap();
        fs::write(staging.join("commit-marker"), "staging").unwrap();
        let started_at = timestamp();
        let journal = MigrationJournal {
            schema: MIGRATION_SCHEMA.to_owned(),
            operation: MigrationOperation::Migrate,
            phase: MigrationPhase::SourceRenamed,
            source: source.clone(),
            destination: destination.clone(),
            backup: backup.clone(),
            staging: Some(staging.clone()),
            preserved: None,
            started_at: started_at.clone(),
            updated_at: started_at,
        };
        let journal_path = journal_path(&source, &destination);
        write_journal(&journal_path, &journal).unwrap();

        let error = rollback_committed_migration(
            &journal,
            &journal_path,
            &staging,
            io::Error::other("destination rename failed"),
        );

        assert!(matches!(error, MigrationError::RollbackFailed { .. }));
        assert!(journal_path.exists());
        assert!(source.join("settings.json").exists());
        assert!(staging.join("commit-marker").exists());

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn rolled_back_journal_cleans_staging_without_recommitting() {
        let source = source_with_data();
        let parent = source.parent().unwrap().to_path_buf();
        let destination = parent.join("migrated-root");
        let backup = parent.join("migration-backup");
        let staging = parent.join("migration-staging");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("commit-marker"), "staging").unwrap();
        let started_at = timestamp();
        let journal_path = journal_path(&source, &destination);
        write_journal(
            &journal_path,
            &MigrationJournal {
                schema: MIGRATION_SCHEMA.to_owned(),
                operation: MigrationOperation::Migrate,
                phase: MigrationPhase::RolledBack,
                source: source.clone(),
                destination: destination.clone(),
                backup: backup.clone(),
                staging: Some(staging.clone()),
                preserved: None,
                started_at: started_at.clone(),
                updated_at: started_at,
            },
        )
        .unwrap();

        let error = migrate_data_root(source.clone(), destination.clone(), Some(backup.clone()))
            .unwrap_err();

        assert!(matches!(error, MigrationError::CommitRolledBack(_)));
        assert!(source.join("settings.json").exists());
        assert!(!destination.exists());
        assert!(!backup.exists());
        assert!(!staging.exists());
        assert!(!journal_path.exists());

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn rollback_destination_renamed_journal_can_be_recovered() {
        let source = source_with_data();
        let parent = source.parent().unwrap().to_path_buf();
        let destination = parent.join("migrated-root");
        let backup = parent.join("migration-backup");
        migrate_data_root(source.clone(), destination.clone(), Some(backup.clone())).unwrap();
        fs::write(destination.join("changed-after-migration"), "new data").unwrap();
        let preserved = parent.join("rollback-preserved");
        let journal_path = journal_path(&source, &destination);
        let started_at = timestamp();
        let report = rollback_report(&source, &destination, &backup, &preserved, &started_at);
        write_journal(
            &journal_path,
            &MigrationJournal {
                schema: MIGRATION_SCHEMA.to_owned(),
                operation: MigrationOperation::Rollback,
                phase: MigrationPhase::DestinationRenamed,
                source: source.clone(),
                destination: destination.clone(),
                backup: backup.clone(),
                staging: None,
                preserved: Some(preserved.clone()),
                started_at: started_at.clone(),
                updated_at: started_at,
            },
        )
        .unwrap();
        fs::rename(&destination, &preserved).unwrap();
        write_report(&preserved.join("migration-rollback-report.json"), &report).unwrap();

        let result =
            rollback_data_root(source.clone(), destination.clone(), backup.clone()).unwrap();

        assert!(source.join("settings.json").exists());
        assert!(!destination.exists());
        assert!(!backup.exists());
        assert!(preserved.join("changed-after-migration").exists());
        assert!(!journal_path.exists());
        assert_eq!(
            result.report_path,
            preserved.join("migration-rollback-report.json")
        );

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn rollback_source_restored_journal_can_finish_after_interruption() {
        let source = source_with_data();
        let parent = source.parent().unwrap().to_path_buf();
        let destination = parent.join("migrated-root");
        let backup = parent.join("migration-backup");
        migrate_data_root(source.clone(), destination.clone(), Some(backup.clone())).unwrap();
        fs::write(destination.join("changed-after-migration"), "new data").unwrap();
        let preserved = parent.join("rollback-preserved");
        let journal_path = journal_path(&source, &destination);
        let started_at = timestamp();
        let report = rollback_report(&source, &destination, &backup, &preserved, &started_at);
        write_journal(
            &journal_path,
            &MigrationJournal {
                schema: MIGRATION_SCHEMA.to_owned(),
                operation: MigrationOperation::Rollback,
                phase: MigrationPhase::DestinationRenamed,
                source: source.clone(),
                destination: destination.clone(),
                backup: backup.clone(),
                staging: None,
                preserved: Some(preserved.clone()),
                started_at: started_at.clone(),
                updated_at: started_at,
            },
        )
        .unwrap();
        fs::rename(&destination, &preserved).unwrap();
        write_report(&preserved.join("migration-rollback-report.json"), &report).unwrap();
        fs::rename(&backup, &source).unwrap();

        rollback_data_root(source.clone(), destination.clone(), backup.clone()).unwrap();

        assert!(source.join("settings.json").exists());
        assert!(!destination.exists());
        assert!(!backup.exists());
        assert!(preserved.join("changed-after-migration").exists());
        assert!(!journal_path.exists());

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn rollback_recovers_when_report_write_was_interrupted() {
        let source = source_with_data();
        let parent = source.parent().unwrap().to_path_buf();
        let destination = parent.join("migrated-root");
        let backup = parent.join("migration-backup");
        migrate_data_root(source.clone(), destination.clone(), Some(backup.clone())).unwrap();
        fs::write(destination.join("changed-after-migration"), "new data").unwrap();
        let preserved = parent.join("rollback-preserved");
        let journal_path = journal_path(&source, &destination);
        let started_at = timestamp();
        write_journal(
            &journal_path,
            &MigrationJournal {
                schema: MIGRATION_SCHEMA.to_owned(),
                operation: MigrationOperation::Rollback,
                phase: MigrationPhase::Prepared,
                source: source.clone(),
                destination: destination.clone(),
                backup: backup.clone(),
                staging: None,
                preserved: Some(preserved.clone()),
                started_at: started_at.clone(),
                updated_at: started_at,
            },
        )
        .unwrap();
        fs::rename(&destination, &preserved).unwrap();

        let result =
            rollback_data_root(source.clone(), destination.clone(), backup.clone()).unwrap();

        assert!(source.join("settings.json").exists());
        assert!(!destination.exists());
        assert!(!backup.exists());
        assert!(preserved.join("changed-after-migration").exists());
        assert!(result.report_path.exists());
        let report: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(result.report_path).unwrap()).unwrap();
        assert_eq!(report["status"], "completed");
        assert!(!journal_path.exists());

        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn migration_rejects_a_running_source_data_root() {
        use std::os::fd::AsRawFd;

        let source = source_with_data();
        let parent = source.parent().unwrap().to_path_buf();
        let destination = parent.join("migrated-root");
        let backup = parent.join("migration-backup");
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(source.join("studio.lock"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );

        let error = migrate_data_root(source.clone(), destination.clone(), Some(backup.clone()))
            .unwrap_err();

        assert!(matches!(error, MigrationError::Preparation(_)));
        assert!(source.join("settings.json").exists());
        assert!(!destination.exists());
        assert!(!backup.exists());
        drop(lock);

        migrate_data_root(source, destination, Some(backup)).unwrap();

        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rollback_rejects_a_running_destination_data_root() {
        use std::os::fd::AsRawFd;

        let source = source_with_data();
        let parent = source.parent().unwrap().to_path_buf();
        let destination = parent.join("migrated-root");
        let backup = parent.join("migration-backup");
        migrate_data_root(source.clone(), destination.clone(), Some(backup.clone())).unwrap();
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(destination.join("studio.lock"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );

        let error =
            rollback_data_root(source.clone(), destination.clone(), backup.clone()).unwrap_err();

        assert!(matches!(error, MigrationError::Preparation(_)));
        assert!(!source.exists());
        assert!(destination.join("settings.json").exists());
        assert!(backup.join("settings.json").exists());
        drop(lock);

        rollback_data_root(source, destination, backup).unwrap();

        fs::remove_dir_all(parent).unwrap();
    }
}
