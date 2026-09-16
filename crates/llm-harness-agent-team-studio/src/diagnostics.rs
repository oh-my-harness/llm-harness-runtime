//! Crash diagnostics for the Studio host process.

use chrono::{SecondsFormat, Utc};
use parking_lot::Mutex;
use serde_json::json;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::{fs, io::Write, process, thread};

static REPORTING_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static REPORT_LOCK: Mutex<()> = Mutex::new(());
static LOG_LOCK: Mutex<()> = Mutex::new(());
static ROTATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const CRASH_SCHEMA: &str = "llm-harness.studio.crash-report.v1";
const LIFECYCLE_SCHEMA: &str = "llm-harness.studio.lifecycle-event.v1";
const LIFECYCLE_FILE: &str = "runtime.jsonl";
const LIFECYCLE_MAX_BYTES: u64 = 1024 * 1024;
const LIFECYCLE_RETAINED_FILES: usize = 10;

/// Install a global panic hook that appends a structured report before the
/// previous hook prints the standard panic message.
///
/// Reports are best-effort so a panic in diagnostics cannot recursively panic
/// during process teardown. The previous hook is still invoked, preserving the
/// normal stderr output expected by supervisors and local developers.
pub fn install_panic_reporter(data_root: impl Into<PathBuf>) {
    let data_root = data_root.into();
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write_report(&data_root, info);
        previous_hook(info);
    }));
}

fn write_report(data_root: &Path, info: &PanicHookInfo<'_>) {
    if REPORTING_IN_PROGRESS.swap(true, Ordering::SeqCst) {
        return;
    }
    let _guard = REPORT_LOCK.lock();
    let report = json!({
        "schema": CRASH_SCHEMA,
        "event_id": uuid::Uuid::new_v4().to_string(),
        "timestamp": Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true),
        "process_id": process::id(),
        "data_root": data_root,
        "thread": thread::current().name(),
        "payload": payload(info),
        "location": info.location().map(|location| json!({
            "file": location.file(),
            "line": location.line(),
            "column": location.column(),
        })),
        "backtrace": std::backtrace::Backtrace::force_capture().to_string(),
    });
    let result = append_private_jsonl(&log_dir(data_root).join("crash.jsonl"), &report);
    if let Err(error) = result {
        eprintln!("agent-team-studio: failed to persist crash report: {error}; report: {report}");
    }
    REPORTING_IN_PROGRESS.store(false, Ordering::SeqCst);
}

fn payload(info: &PanicHookInfo<'_>) -> String {
    if let Some(message) = info.payload().downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = info.payload().downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_owned()
    }
}

fn log_dir(data_root: &Path) -> PathBuf {
    data_root.join("logs")
}

pub fn append_lifecycle_event(
    data_root: impl Into<PathBuf>,
    data: serde_json::Value,
) -> std::io::Result<()> {
    let data_root = data_root.into();
    let event = json!({
        "schema": LIFECYCLE_SCHEMA,
        "event_id": uuid::Uuid::new_v4().to_string(),
        "timestamp": Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true),
        "process_id": process::id(),
        "data_root": data_root,
        "data": data,
    });
    append_private_jsonl(&log_dir(&data_root).join(LIFECYCLE_FILE), &event)
}

pub fn read_lifecycle_events(
    data_root: impl AsRef<Path>,
    limit: usize,
) -> std::io::Result<Vec<serde_json::Value>> {
    let path = log_dir(data_root.as_ref()).join(LIFECYCLE_FILE);
    let contents = fs::read_to_string(path)?;
    let mut events = Vec::new();
    for line in contents.lines().rev().take(limit) {
        if line.trim().is_empty() {
            continue;
        }
        let event: serde_json::Value = serde_json::from_str(line)?;
        events.push(event);
    }
    Ok(events)
}

fn append_private_jsonl(path: &Path, event: &serde_json::Value) -> std::io::Result<()> {
    let _guard = LOG_LOCK.lock();
    rotate_if_needed(path)?;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    #[cfg(unix)]
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    #[cfg(not(unix))]
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    set_private_permissions(path, &mut file)?;
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    file.write_all(&line)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn rotate_if_needed(path: &Path) -> std::io::Result<()> {
    let log_dir = path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid log path"))?;
    fs::create_dir_all(log_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(log_dir, fs::Permissions::from_mode(0o700))?;
    }

    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.len() < LIFECYCLE_MAX_BYTES {
        return Ok(());
    }

    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("runtime");
    let sequence = ROTATION_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
    let rotated_name = format!(
        "{stem}-{}-{}-{sequence}.jsonl",
        Utc::now().format("%Y%m%dT%H%M%S%6fZ"),
        process::id(),
    );
    fs::rename(path, log_dir.join(rotated_name))?;

    let mut rotated: Vec<PathBuf> = fs::read_dir(log_dir)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|candidate| {
            candidate != path
                && candidate
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
                && candidate
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|candidate_stem| candidate_stem.starts_with(&format!("{stem}-")))
        })
        .collect();
    rotated.sort();
    while rotated.len() > LIFECYCLE_RETAINED_FILES {
        let oldest = rotated.remove(0);
        let _ = fs::remove_file(oldest);
    }
    Ok(())
}

fn set_private_permissions(path: &Path, _file: &mut fs::File) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(test: &str) -> PathBuf {
        let temp = std::env::temp_dir().join(format!(
            "agent-team-studio-diagnostics-{test}-{}-{}",
            process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        fs::create_dir_all(&temp).unwrap();
        temp
    }

    #[test]
    fn panic_hook_appends_private_structured_report() {
        let root = temp_root("panic");
        install_panic_reporter(&root);
        let handle = thread::Builder::new()
            .name("studio-crash-test".to_owned())
            .spawn(move || panic!("expected panic"))
            .unwrap();
        assert!(handle.join().is_err());

        let path = log_dir(&root).join("crash.jsonl");
        let contents = fs::read_to_string(&path).unwrap();
        let report: serde_json::Value = contents.lines().last().unwrap().parse().unwrap();
        assert_eq!(report["schema"], CRASH_SCHEMA);
        assert_eq!(report["thread"], "studio-crash-test");
        assert_eq!(report["payload"], "expected panic");
        assert!(report["event_id"].as_str().is_some());
        assert!(report["timestamp"].as_str().is_some());
        assert!(!report["backtrace"].as_str().unwrap().trim().is_empty());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn lifecycle_events_are_private_structured_jsonl() {
        let root = temp_root("lifecycle");
        append_lifecycle_event(
            &root,
            json!({"event": "startup", "recovery": {"status": "healthy"}}),
        )
        .unwrap();

        let path = log_dir(&root).join(LIFECYCLE_FILE);
        let contents = fs::read_to_string(&path).unwrap();
        let event: serde_json::Value = contents.lines().last().unwrap().parse().unwrap();
        assert_eq!(event["schema"], LIFECYCLE_SCHEMA);
        assert_eq!(event["data"]["event"], "startup");
        assert!(event["event_id"].as_str().is_some());
        assert!(event["timestamp"].as_str().is_some());
        assert_eq!(event["process_id"], process::id());
        assert_eq!(event["data_root"], root.to_string_lossy().into_owned());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn lifecycle_rotation_bounds_retained_files() {
        let root = temp_root("rotation");
        let path = log_dir(&root).join(LIFECYCLE_FILE);
        fs::create_dir_all(log_dir(&root)).unwrap();
        fs::write(&path, vec![b'x'; LIFECYCLE_MAX_BYTES as usize]).unwrap();
        for index in 0..LIFECYCLE_RETAINED_FILES {
            let name = format!("runtime-20200101T000000000000Z-{index}-{index}.jsonl");
            fs::write(log_dir(&root).join(name), "old\n").unwrap();
        }

        append_lifecycle_event(&root, json!({"event": "rotation"})).unwrap();
        let path = log_dir(&root).join(LIFECYCLE_FILE);
        let mut rotated: Vec<_> = fs::read_dir(log_dir(&root))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("runtime-") && name != LIFECYCLE_FILE)
            .collect();
        rotated.sort();
        assert_eq!(rotated.len(), LIFECYCLE_RETAINED_FILES);
        assert!(!rotated.iter().any(|name| name.ends_with("-0-0.jsonl")));
        assert!(fs::metadata(&path).unwrap().len() > 0);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
