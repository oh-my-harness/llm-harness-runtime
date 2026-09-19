use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

const DIAGNOSTICS_BUNDLE_SCHEMA: &str = "llm-harness.runtime-app.diagnostics-bundle.v1";
const LIFECYCLE_CURRENT_FILE: &str = "runtime.jsonl";
const CRASH_CURRENT_FILE: &str = "crash.jsonl";
const LIFECYCLE_EVENT_LIMIT: usize = 100;
const CRASH_EVENT_LIMIT: usize = 20;
const MAX_LOG_FILES_PER_KIND: usize = 11;
const MAX_LOG_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SAFE_STRING_LENGTH: usize = 256;
#[cfg(windows)]
const WINDOWS_FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

#[derive(Debug, Error)]
pub enum DiagnosticsError {
    #[error("usage: {0}")]
    Usage(String),
    #[error("data root must be an existing directory without symlinks: {0}")]
    InvalidDataRoot(PathBuf),
    #[error("logs directory must be an existing directory without symlinks: {0}")]
    InvalidLogsDirectory(PathBuf),
    #[error("log entry must be a regular file without symlinks: {0}")]
    InvalidLogEntry(PathBuf),
    #[error("log file exceeds the {MAX_LOG_FILE_BYTES} byte safety limit: {0}")]
    LogFileTooLarge(PathBuf),
    #[error("logs contain more than {MAX_LOG_FILES_PER_KIND} matching files for one kind: {0}")]
    TooManyLogFiles(PathBuf),
    #[error("diagnostics output already exists: {0}")]
    OutputExists(PathBuf),
    #[error("diagnostics output parent must be an existing directory without symlinks: {0}")]
    InvalidOutputParent(PathBuf),
    #[error("diagnostics output cannot be inside the data root: {0}")]
    OutputInsideDataRoot(PathBuf),
    #[error("failed to read diagnostics logs: {0}")]
    Read(#[source] io::Error),
    #[error("failed to write diagnostics bundle: {0}")]
    Write(#[source] io::Error),
    #[error("failed to serialize diagnostics bundle: {0}")]
    Serialize(#[source] serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct DiagnosticsCommand {
    pub data_root: PathBuf,
    pub output: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct DiagnosticsResult {
    pub operation: &'static str,
    pub output_path: PathBuf,
    pub schema: &'static str,
}

#[derive(Debug, Serialize)]
struct DiagnosticsBundle {
    schema: &'static str,
    generated_at: String,
    data_root: DataRootDigest,
    lifecycle: DiagnosticsSection,
    crash: DiagnosticsSection,
}

#[derive(Debug, Serialize)]
struct DataRootDigest {
    sha256: String,
    length: usize,
}

#[derive(Debug, Serialize)]
struct DiagnosticsSection {
    events: Vec<Value>,
    files: Vec<DiagnosticsFile>,
    truncated: bool,
    invalid_lines: usize,
}

#[derive(Debug, Serialize)]
struct DiagnosticsFile {
    kind: &'static str,
    size_bytes: u64,
    parsed_events: usize,
    invalid_lines: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogKind {
    Lifecycle,
    Crash,
}

impl LogKind {
    fn current_file(self) -> &'static str {
        match self {
            Self::Lifecycle => LIFECYCLE_CURRENT_FILE,
            Self::Crash => CRASH_CURRENT_FILE,
        }
    }

    fn event_limit(self) -> usize {
        match self {
            Self::Lifecycle => LIFECYCLE_EVENT_LIMIT,
            Self::Crash => CRASH_EVENT_LIMIT,
        }
    }
}

pub fn parse_diagnostics_command(
    arguments: &[String],
) -> Result<Option<DiagnosticsCommand>, DiagnosticsError> {
    let Some(command_name) = arguments.first() else {
        return Ok(None);
    };
    if command_name != "collect-diagnostics" {
        return Ok(None);
    }

    let data_root = required_flag(arguments, "--data-root")?;
    let output = required_flag(arguments, "--output")?;
    reject_remaining_arguments(arguments, &["--data-root", "--output"])?;
    Ok(Some(DiagnosticsCommand {
        data_root: data_root.into(),
        output: output.into(),
    }))
}

pub fn run_diagnostics_command(
    command: DiagnosticsCommand,
) -> Result<DiagnosticsResult, DiagnosticsError> {
    let bundle = collect_diagnostics_bundle(&command.data_root, &command.output)?;
    Ok(DiagnosticsResult {
        operation: "collect-diagnostics",
        output_path: command.output,
        schema: bundle.schema,
    })
}

fn collect_diagnostics_bundle(
    data_root: &Path,
    output: &Path,
) -> Result<DiagnosticsBundle, DiagnosticsError> {
    let data_root = validate_data_root(data_root)?;
    let output = validate_output_path(output, &data_root)?;
    let logs_directory = validate_logs_directory(&data_root)?;

    let lifecycle = collect_section(&logs_directory, LogKind::Lifecycle)?;
    let crash = collect_section(&logs_directory, LogKind::Crash)?;
    let bundle = DiagnosticsBundle {
        schema: DIAGNOSTICS_BUNDLE_SCHEMA,
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true),
        data_root: data_root_digest(&data_root),
        lifecycle,
        crash,
    };
    write_bundle(&output, &bundle)?;
    Ok(bundle)
}

fn validate_data_root(data_root: &Path) -> Result<PathBuf, DiagnosticsError> {
    let symlink_metadata = fs::symlink_metadata(data_root)
        .map_err(|_| DiagnosticsError::InvalidDataRoot(data_root.to_owned()))?;
    if symlink_metadata.file_type().is_symlink() || !symlink_metadata.is_dir() {
        return Err(DiagnosticsError::InvalidDataRoot(data_root.to_owned()));
    }
    data_root
        .canonicalize()
        .map_err(|_| DiagnosticsError::InvalidDataRoot(data_root.to_owned()))
}

fn validate_logs_directory(data_root: &Path) -> Result<PathBuf, DiagnosticsError> {
    let logs_directory = data_root.join("logs");
    match fs::symlink_metadata(&logs_directory) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(logs_directory),
        Err(_) => return Err(DiagnosticsError::InvalidLogsDirectory(logs_directory)),
        Ok(symlink_metadata) => {
            if symlink_metadata.file_type().is_symlink() || !symlink_metadata.is_dir() {
                return Err(DiagnosticsError::InvalidLogsDirectory(logs_directory));
            }
        }
    }
    let symlink_metadata = fs::metadata(&logs_directory)
        .map_err(|_| DiagnosticsError::InvalidLogsDirectory(logs_directory.clone()))?;
    if symlink_metadata.file_type().is_symlink() || !symlink_metadata.is_dir() {
        return Err(DiagnosticsError::InvalidLogsDirectory(logs_directory));
    }
    logs_directory
        .canonicalize()
        .map_err(|_| DiagnosticsError::InvalidLogsDirectory(logs_directory))
}

fn validate_output_path(output: &Path, data_root: &Path) -> Result<PathBuf, DiagnosticsError> {
    if fs::symlink_metadata(output).is_ok() {
        return Err(DiagnosticsError::OutputExists(output.to_owned()));
    }
    let parent = match output.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let symlink_metadata = fs::symlink_metadata(parent)
        .map_err(|_| DiagnosticsError::InvalidOutputParent(parent.to_owned()))?;
    if symlink_metadata.file_type().is_symlink() || !symlink_metadata.is_dir() {
        return Err(DiagnosticsError::InvalidOutputParent(parent.to_owned()));
    }
    let parent = parent
        .canonicalize()
        .map_err(|_| DiagnosticsError::InvalidOutputParent(parent.to_owned()))?;
    if parent.starts_with(data_root) {
        return Err(DiagnosticsError::OutputInsideDataRoot(output.to_owned()));
    }
    Ok(output.to_owned())
}

fn collect_section(
    logs_directory: &Path,
    kind: LogKind,
) -> Result<DiagnosticsSection, DiagnosticsError> {
    if !logs_directory.exists() {
        return Ok(DiagnosticsSection {
            events: Vec::new(),
            files: Vec::new(),
            truncated: false,
            invalid_lines: 0,
        });
    }

    let mut files = Vec::new();
    let mut events = Vec::new();
    let mut invalid_lines = 0usize;
    for path in log_files(logs_directory, kind)? {
        let kind_label = if path.file_name().map(|name| name == kind.current_file()) == Some(true) {
            "current"
        } else {
            "rotated"
        };
        let (file_events, file_invalid_lines, size_bytes) = read_log_file(&path)?;
        files.push(DiagnosticsFile {
            kind: kind_label,
            size_bytes,
            parsed_events: file_events.len(),
            invalid_lines: file_invalid_lines,
        });
        invalid_lines += file_invalid_lines;
        events.extend(file_events);
    }

    events.sort_by_key(event_timestamp);
    let truncated = events.len() > kind.event_limit();
    if truncated {
        let excess = events.len() - kind.event_limit();
        events.drain(..excess);
    }
    Ok(DiagnosticsSection {
        events: events
            .into_iter()
            .map(|event| redact_event(event, kind))
            .collect(),
        files,
        truncated,
        invalid_lines,
    })
}

fn log_files(logs_directory: &Path, kind: LogKind) -> Result<Vec<PathBuf>, DiagnosticsError> {
    let current = kind.current_file();
    let current_stem = Path::new(current)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    let rotated_prefix = format!("{current_stem}-");
    let mut paths = Vec::new();
    for entry in fs::read_dir(logs_directory).map_err(DiagnosticsError::Read)? {
        let entry = entry.map_err(DiagnosticsError::Read)?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        let is_current = name == current;
        let is_rotated = name.starts_with(&rotated_prefix) && name.ends_with(".jsonl");
        if !is_current && !is_rotated {
            continue;
        }
        let file_type = entry.file_type().map_err(DiagnosticsError::Read)?;
        if file_type.is_symlink() || !file_type.is_file() {
            return Err(DiagnosticsError::InvalidLogEntry(entry.path()));
        }
        paths.push(entry.path());
    }
    paths.sort();
    if paths.len() > MAX_LOG_FILES_PER_KIND {
        return Err(DiagnosticsError::TooManyLogFiles(logs_directory.to_owned()));
    }
    Ok(paths)
}

fn read_log_file(path: &Path) -> Result<(Vec<Value>, usize, u64), DiagnosticsError> {
    let metadata = file_metadata(path)?;
    if metadata.len() > MAX_LOG_FILE_BYTES {
        return Err(DiagnosticsError::LogFileTooLarge(path.to_owned()));
    }
    let file = open_log_file(path)?;
    let size_bytes = file.metadata().map_err(DiagnosticsError::Read)?.len();
    let mut contents = Vec::new();
    file.take(MAX_LOG_FILE_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(DiagnosticsError::Read)?;
    if contents.len() as u64 > MAX_LOG_FILE_BYTES || size_bytes != contents.len() as u64 {
        return Err(DiagnosticsError::LogFileTooLarge(path.to_owned()));
    }
    let contents = String::from_utf8_lossy(&contents);
    let mut events = Vec::new();
    let mut invalid_lines = 0usize;
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(event) if event.is_object() => events.push(event),
            _ => invalid_lines += 1,
        }
    }
    Ok((events, invalid_lines, size_bytes))
}

fn open_log_file(path: &Path) -> Result<File, DiagnosticsError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(WINDOWS_FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path).map_err(DiagnosticsError::Read)
}

fn file_metadata(path: &Path) -> Result<fs::Metadata, DiagnosticsError> {
    let metadata = fs::symlink_metadata(path).map_err(DiagnosticsError::Read)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(DiagnosticsError::InvalidLogEntry(path.to_owned()));
    }
    Ok(metadata)
}

fn event_timestamp(event: &Value) -> String {
    event
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn redact_event(event: Value, kind: LogKind) -> Value {
    match kind {
        LogKind::Lifecycle => redact_lifecycle_event(event),
        LogKind::Crash => redact_crash_event(event),
    }
}

fn redact_lifecycle_event(event: Value) -> Value {
    let mut redacted = json!({});
    copy_string_field(&event, &mut redacted, "schema");
    copy_string_field(&event, &mut redacted, "event_id");
    copy_string_field(&event, &mut redacted, "timestamp");
    copy_u64_field(&event, &mut redacted, "process_id");

    let data = event.get("data").cloned().unwrap_or_else(|| json!({}));
    let mut redacted_data = json!({});
    copy_string_field(&data, &mut redacted_data, "event");
    copy_string_field(&data, &mut redacted_data, "source");
    copy_string_field(&data, &mut redacted_data, "reason");
    copy_bool_field(&data, &mut redacted_data, "startup_aborted");
    copy_u64_field(&data, &mut redacted_data, "configured_timeout_secs");
    copy_u64_field(&data, &mut redacted_data, "effective_timeout_secs");
    redact_field(&data, &mut redacted_data, "error");
    redact_field(&data, &mut redacted_data, "detail");
    redact_field(&data, &mut redacted_data, "bound_address");

    if let Some(recovery) = data.get("recovery") {
        let mut redacted_recovery = json!({});
        copy_string_field(recovery, &mut redacted_recovery, "status");
        copy_u64_field(recovery, &mut redacted_recovery, "persisted_teams");
        copy_u64_field(recovery, &mut redacted_recovery, "restored_teams");
        if let Some(failures) = recovery.get("failed_teams").and_then(Value::as_array) {
            let redacted_failures: Vec<Value> = failures
                .iter()
                .map(|failure| {
                    let mut redacted_failure = json!({});
                    copy_string_field(failure, &mut redacted_failure, "team_id");
                    copy_string_field(failure, &mut redacted_failure, "reason");
                    redact_field(failure, &mut redacted_failure, "detail");
                    redacted_failure
                })
                .collect();
            redacted_recovery["failed_teams"] = json!(redacted_failures);
        }
        redacted_data["recovery"] = redacted_recovery;
    }
    redacted["data"] = redacted_data;
    redacted
}

fn redact_crash_event(event: Value) -> Value {
    let mut redacted = json!({});
    copy_string_field(&event, &mut redacted, "schema");
    copy_string_field(&event, &mut redacted, "event_id");
    copy_string_field(&event, &mut redacted, "timestamp");
    copy_u64_field(&event, &mut redacted, "process_id");
    copy_string_field(&event, &mut redacted, "thread");
    redact_field(&event, &mut redacted, "payload");

    if let Some(location) = event.get("location") {
        let mut redacted_location = json!({});
        redact_field(location, &mut redacted_location, "file");
        copy_u64_field(location, &mut redacted_location, "line");
        copy_u64_field(location, &mut redacted_location, "column");
        redacted["location"] = redacted_location;
    }
    if let Some(backtrace) = event.get("backtrace") {
        redacted["backtrace"] = redact_backtrace(backtrace);
    }
    redacted
}

fn redact_field(source: &Value, destination: &mut Value, field: &str) {
    let Some(value) = source.get(field) else {
        return;
    };
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    destination[field] = json!({
        "redacted": true,
        "sha256": sha256_hex(&bytes),
        "length": bytes.len(),
    });
}

fn redact_backtrace(value: &Value) -> Value {
    let text = value.as_str().unwrap_or_default();
    let line_count = if text.is_empty() {
        0
    } else {
        text.lines().count()
    };
    json!({
        "redacted": true,
        "sha256": sha256_hex(text.as_bytes()),
        "length": text.len(),
        "line_count": line_count,
    })
}

fn data_root_digest(data_root: &Path) -> DataRootDigest {
    let bytes = data_root.as_os_str().as_encoded_bytes();
    DataRootDigest {
        sha256: sha256_hex(bytes),
        length: bytes.len(),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn copy_string_field(source: &Value, destination: &mut Value, field: &str) {
    if let Some(value) = source
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| value.len() <= MAX_SAFE_STRING_LENGTH)
    {
        destination[field] = json!(value);
    }
}

fn copy_u64_field(source: &Value, destination: &mut Value, field: &str) {
    if let Some(value) = source.get(field).and_then(Value::as_u64) {
        destination[field] = json!(value);
    }
}

fn copy_bool_field(source: &Value, destination: &mut Value, field: &str) {
    if let Some(value) = source.get(field).and_then(Value::as_bool) {
        destination[field] = json!(value);
    }
}

fn write_bundle(output: &Path, bundle: &DiagnosticsBundle) -> Result<(), DiagnosticsError> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(WINDOWS_FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = match options.open(output) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(DiagnosticsError::OutputExists(output.to_owned()));
        }
        Err(error) => return Err(DiagnosticsError::Write(error)),
    };
    let result = (|| {
        let mut contents =
            serde_json::to_vec_pretty(bundle).map_err(DiagnosticsError::Serialize)?;
        contents.push(b'\n');
        file.write_all(&contents).map_err(DiagnosticsError::Write)?;
        file.flush().map_err(DiagnosticsError::Write)?;
        file.sync_all().map_err(DiagnosticsError::Write)
    })();
    if result.is_err() {
        drop(file);
        let _ = fs::remove_file(output);
    }
    result?;
    #[cfg(unix)]
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        File::open(parent)
            .and_then(|parent| parent.sync_all())
            .map_err(DiagnosticsError::Write)?;
    }
    Ok(())
}

fn required_flag<'a>(arguments: &'a [String], flag: &str) -> Result<&'a str, DiagnosticsError> {
    let mut values = arguments
        .iter()
        .skip(1)
        .skip_while(|argument| argument.as_str() != flag);
    values.next();
    match values.next() {
        Some(value) if !value.starts_with("--") => Ok(value),
        Some(_) => Err(DiagnosticsError::Usage(format!("{flag} requires a value"))),
        None => Err(DiagnosticsError::Usage(format!("missing {flag}"))),
    }
}

fn reject_remaining_arguments(
    arguments: &[String],
    known_flags: &[&str],
) -> Result<(), DiagnosticsError> {
    for flag in known_flags {
        let count = arguments
            .iter()
            .skip(1)
            .filter(|argument| argument.as_str() == *flag)
            .count();
        if count > 1 {
            return Err(DiagnosticsError::Usage(format!("duplicate flag: {flag}")));
        }
    }
    let mut index = 1;
    while index < arguments.len() {
        if !known_flags.contains(&arguments[index].as_str()) {
            return Err(DiagnosticsError::Usage(format!(
                "unexpected argument: {}",
                arguments[index]
            )));
        }
        index += 2;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(test: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "runtime-app-diagnostics-{test}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn parses_diagnostics_command_strictly() {
        let arguments = vec![
            "collect-diagnostics".to_owned(),
            "--data-root".to_owned(),
            "/tmp/data".to_owned(),
            "--output".to_owned(),
            "/tmp/bundle.json".to_owned(),
        ];
        let command = parse_diagnostics_command(&arguments).unwrap().unwrap();
        assert_eq!(command.data_root, Path::new("/tmp/data"));
        assert_eq!(command.output, Path::new("/tmp/bundle.json"));

        let invalid = vec![
            "collect-diagnostics".to_owned(),
            "--data-root".to_owned(),
            "/tmp/data".to_owned(),
            "--output".to_owned(),
            "/tmp/bundle.json".to_owned(),
            "--extra".to_owned(),
        ];
        assert!(parse_diagnostics_command(&invalid).is_err());

        let duplicate = vec![
            "collect-diagnostics".to_owned(),
            "--data-root".to_owned(),
            "/tmp/data".to_owned(),
            "--data-root".to_owned(),
            "/tmp/other".to_owned(),
            "--output".to_owned(),
            "/tmp/bundle.json".to_owned(),
        ];
        assert!(parse_diagnostics_command(&duplicate).is_err());

        let missing = vec![
            "collect-diagnostics".to_owned(),
            "--data-root".to_owned(),
            "/tmp/data".to_owned(),
        ];
        assert!(parse_diagnostics_command(&missing).is_err());
    }

    #[test]
    fn collects_redacts_and_limits_lifecycle_and_crash_logs() {
        let root = temp_root("collect");
        let logs = root.join("logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(
            logs.join("runtime-older.jsonl"),
            String::from(
                "{{\"schema\":\"llm-harness.studio.lifecycle-event.v1\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"data_root\":\"secret-root\",\"event_id\":\"old\",\"process_id\":1,\"data\":{{\"event\":\"startup\",\"api_key\":\"secret-token\",\"bound_address\":\"127.0.0.1:12345\",\"error\":\"secret-error\",\"recovery\":{{\"status\":\"degraded\",\"persisted_teams\":2,\"restored_teams\":1,\"failed_teams\":[{{\"team_id\":\"team\",\"reason\":\"workspace_check\",\"detail\":\"workspace-secret\"}}]}}}}}}\n"
            ),
        )
        .unwrap();
        let mut lifecycle_current = String::new();
        for index in 0..105 {
            lifecycle_current.push_str(
                &format!(
                    "{{\"schema\":\"llm-harness.studio.lifecycle-event.v1\",\"timestamp\":\"2026-01-02T00:{index:02}:00Z\",\"event_id\":\"event-{index}\",\"process_id\":2,\"data_root\":\"secret-root\",\"data\":{{\"event\":\"shutdown_signal\",\"source\":\"sigterm\",\"detail\":\"secret-detail\",\"bound_address\":\"127.0.0.1:12345\"}}}}\n"
                ),
            );
        }
        fs::write(logs.join("runtime.jsonl"), lifecycle_current).unwrap();
        fs::write(
            logs.join("crash.jsonl"),
            "{\"schema\":\"llm-harness.studio.crash-report.v1\",\"timestamp\":\"2026-01-03T00:00:00Z\",\"event_id\":\"crash\",\"process_id\":3,\"thread\":\"main\",\"data_root\":\"secret-root\",\"payload\":\"secret-panic\",\"location\":{\"file\":\"secret-source.rs\",\"line\":10,\"column\":2},\"backtrace\":\"secret line 1\\nsecret line 2\"}\n",
        )
        .unwrap();
        fs::write(
            logs.join("crash-older.jsonl"),
            "{\"schema\":\"llm-harness.studio.crash-report.v1\",\"timestamp\":\"2026-01-02T00:00:00Z\",\"event_id\":\"old-crash\",\"process_id\":3,\"payload\":\"old-secret-panic\"}\n",
        )
        .unwrap();

        let parent = temp_root("collect-output");
        let output = parent.join("bundle.json");
        let bundle = collect_diagnostics_bundle(&root, &output).unwrap();
        assert_eq!(bundle.schema, DIAGNOSTICS_BUNDLE_SCHEMA);
        assert_eq!(bundle.lifecycle.events.len(), LIFECYCLE_EVENT_LIMIT);
        assert!(bundle.lifecycle.truncated);
        assert_eq!(bundle.lifecycle.events[0]["event_id"], "event-5");
        assert_eq!(bundle.crash.events.len(), 2);
        assert_eq!(bundle.crash.events[0]["event_id"], "old-crash");
        assert_eq!(bundle.crash.events[1]["event_id"], "crash");
        assert_eq!(bundle.crash.files.len(), 2);

        let contents = fs::read_to_string(&output).unwrap();
        assert!(!contents.contains("secret-root"));
        assert!(!contents.contains("secret-token"));
        assert!(!contents.contains("secret-error"));
        assert!(!contents.contains("secret-detail"));
        assert!(!contents.contains("workspace-secret"));
        assert!(!contents.contains("127.0.0.1:12345"));
        assert!(!contents.contains("secret-panic"));
        assert!(!contents.contains("old-secret-panic"));
        assert!(!contents.contains("secret-source.rs"));
        assert!(!contents.contains("secret line 1"));
        let bundle: Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(
            bundle["lifecycle"]["events"][0]["data"]["bound_address"]["redacted"],
            true
        );
        assert_eq!(bundle["crash"]["events"][1]["backtrace"]["line_count"], 2);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&output).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&parent).unwrap();
    }

    #[test]
    fn rejects_existing_output_and_output_inside_data_root() {
        let root = temp_root("output-validation");
        let output = root.join("bundle.json");
        fs::write(&output, "existing").unwrap();
        let error = collect_diagnostics_bundle(&root, &output.parent().unwrap().join("other.json"))
            .unwrap_err();
        assert!(matches!(error, DiagnosticsError::OutputInsideDataRoot(_)));
        assert!(matches!(
            collect_diagnostics_bundle(&root, &output),
            Err(DiagnosticsError::OutputExists(_))
        ));
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_logs() {
        let root = temp_root("symlink");
        let target = temp_root("symlink-target");
        fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, root.join("logs")).unwrap();
        let output = target.join("bundle.json");
        assert!(matches!(
            collect_diagnostics_bundle(&root, &output),
            Err(DiagnosticsError::InvalidLogsDirectory(_))
        ));
        fs::remove_file(root.join("logs")).unwrap();
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&target).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_log_file() {
        let root = temp_root("symlink-file");
        let logs = root.join("logs");
        fs::create_dir_all(&logs).unwrap();
        let target = root.join("runtime-target.jsonl");
        fs::write(&target, "{}\n").unwrap();
        std::os::unix::fs::symlink(&target, logs.join("runtime.jsonl")).unwrap();
        let output = temp_root("symlink-file-output").join("bundle.json");
        assert!(matches!(
            collect_diagnostics_bundle(&root, &output),
            Err(DiagnosticsError::InvalidLogEntry(_))
        ));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rejects_too_many_matching_log_files() {
        let root = temp_root("too-many");
        let logs = root.join("logs");
        fs::create_dir_all(&logs).unwrap();
        for index in 0..=MAX_LOG_FILES_PER_KIND {
            fs::write(logs.join(format!("runtime-{index}.jsonl")), "{}\n").unwrap();
        }
        let output = temp_root("too-many-output").join("bundle.json");
        assert!(matches!(
            collect_diagnostics_bundle(&root, &output),
            Err(DiagnosticsError::TooManyLogFiles(_))
        ));
        fs::remove_dir_all(&root).unwrap();
    }
}
