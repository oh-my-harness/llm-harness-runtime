//! Crash diagnostics for the Studio host process.

use chrono::{SecondsFormat, Utc};
use parking_lot::Mutex;
use serde_json::json;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{fs, io::Write, process, thread};

static REPORTING_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static REPORT_LOCK: Mutex<()> = Mutex::new(());

const CRASH_SCHEMA: &str = "llm-harness.studio.crash-report.v1";

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
    let result = fs::create_dir_all(log_dir(data_root))
        .and_then(|()| append_report(log_dir(data_root), &report));
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

fn append_report(log_dir: PathBuf, report: &serde_json::Value) -> std::io::Result<()> {
    let path = log_dir.join("crash.jsonl");
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    #[cfg(unix)]
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)?;
    #[cfg(not(unix))]
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    set_private_permissions(&path, &mut file)?;
    serde_json::to_writer(&mut file, report)?;
    file.write_all(b"\n")?;
    file.flush()?;
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
        std::env::temp_dir().join(format!(
            "agent-team-studio-diagnostics-{test}-{}-{}",
            process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ))
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
}
