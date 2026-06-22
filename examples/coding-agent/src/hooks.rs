use futures::future::BoxFuture;
use llm_harness_runtime::human_approval::{ApprovalDecision, ApprovalRequest, HumanApprover};
use tokio::sync::Mutex;

/// Prompts the user via `/dev/tty` before executing a tool.
///
/// Uses `/dev/tty` rather than stdin so it works even when stdin is held by
/// rustyline or redirected from a pipe.
///
/// A global mutex serialises blocking I/O on `/dev/tty` so that parallel tool
/// invocations (from a single assistant message with multiple `tool_use`
/// blocks) show confirm prompts one at a time instead of racing on the shared
/// terminal device.
pub struct TtyHumanApprover {
    confirm_tools: Vec<String>,
}

impl TtyHumanApprover {
    /// Create an approver that requires confirmation before the given tools.
    pub fn new(confirm_tools: Vec<&str>) -> Self {
        Self {
            confirm_tools: confirm_tools.iter().map(|s| (*s).to_owned()).collect(),
        }
    }
}

impl HumanApprover for TtyHumanApprover {
    fn ask<'a>(&'a self, request: &'a ApprovalRequest) -> BoxFuture<'a, Option<ApprovalDecision>> {
        let needs_confirm = self.confirm_tools.iter().any(|t| t == &request.tool_name);
        let tool_name = request.tool_name.clone();
        let args = request.arguments.clone();

        Box::pin(async move {
            if !needs_confirm {
                return Some(ApprovalDecision::Approve);
            }

            // Serialise concurrent confirm prompts so they don't race on
            // `/dev/tty`. The lock is held at the async layer, before
            // `spawn_blocking`, ensuring only one blocking I/O at a time.
            let _guard = TTY_LOCK.lock().await;

            let args_summary = format_args_summary(&tool_name, &args);
            let prompt = format!(
                "\n[confirm] {} {}\nProceed? [y/N] ",
                tool_name, args_summary
            );

            tokio::task::spawn_blocking(move || {
                use std::io::{BufRead, BufReader, Write};

                let mut tty = match std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open("/dev/tty")
                {
                    Ok(f) => f,
                    Err(e) => {
                        // No controlling terminal (CI, Docker, piped). Approve and warn so
                        // the agent is not silently blocked with a misleading "no response" error.
                        eprintln!("[warn] cannot open /dev/tty ({e}); auto-approving tool call");
                        return Some(ApprovalDecision::Approve);
                    }
                };

                if write!(tty, "{}", prompt).is_err() || tty.flush().is_err() {
                    eprintln!(
                        "[warn] failed to write confirmation prompt; auto-approving tool call"
                    );
                    return Some(ApprovalDecision::Approve);
                }

                let mut line = String::new();
                match BufReader::new(tty).read_line(&mut line) {
                    Ok(0) | Err(_) => None, // EOF or read error — let TimeoutPolicy decide
                    Ok(_) => {
                        if line.trim().eq_ignore_ascii_case("y") {
                            Some(ApprovalDecision::Approve)
                        } else {
                            Some(ApprovalDecision::Deny {
                                reason: "User declined".into(),
                            })
                        }
                    }
                }
            })
            .await
            .ok()
            .flatten()
        })
    }
}

/// Serialises `/dev/tty` access across concurrent `ask()` calls.
static TTY_LOCK: Mutex<()> = Mutex::const_new(());

fn format_args_summary(tool_name: &str, args: &serde_json::Value) -> String {
    match tool_name {
        "bash" => args["command"]
            .as_str()
            .map(|s| format!("`{}`", truncate(s, 120)))
            .unwrap_or_default(),
        "edit" | "write" => args["file_path"]
            .as_str()
            .map(|s| format!("`{}`", s))
            .unwrap_or_default(),
        _ => {
            let s = args.to_string();
            format!("`{}`", truncate(&s, 120))
        }
    }
}

fn truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        s
    } else {
        &s[..s.floor_char_boundary(max_bytes)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_request(tool_name: &str, args: serde_json::Value) -> ApprovalRequest {
        ApprovalRequest {
            request_id: "test-id".into(),
            tool_name: tool_name.to_owned(),
            arguments: args,
            context: String::new(),
        }
    }

    #[tokio::test]
    async fn non_listed_tool_auto_approves() {
        let approver = TtyHumanApprover::new(vec!["bash", "edit", "write"]);
        let request = make_request("read", serde_json::json!({"file_path": "foo.rs"}));
        let decision = approver.ask(&request).await;
        assert!(matches!(decision, Some(ApprovalDecision::Approve)));
    }

    #[test]
    fn format_args_bash_shows_command() {
        let args = serde_json::json!({"command": "echo hello"});
        let s = format_args_summary("bash", &args);
        assert!(s.contains("echo hello"));
    }

    #[test]
    fn format_args_edit_shows_file_path() {
        let args = serde_json::json!({"file_path": "src/main.rs", "old_string": "foo", "new_string": "bar"});
        let s = format_args_summary("edit", &args);
        assert!(s.contains("src/main.rs"));
    }

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("hello", 100), "hello");
    }

    #[test]
    fn truncate_long_string_cut_at_boundary() {
        let s = "hello world";
        assert_eq!(truncate(s, 5), "hello");
    }
}
