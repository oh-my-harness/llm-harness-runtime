//! Structured deny message builders for ToolFailure.

use llm_harness_types::ToolFailure;

use crate::safety::command::SafetyVerdict;
use crate::safety::path::PathViolation;

/// Truncate raw input to 200 chars to avoid context pollution.
const MAX_RAW_INPUT_LEN: usize = 200;

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_RAW_INPUT_LEN {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(MAX_RAW_INPUT_LEN).collect();
        format!("{truncated}... (truncated)")
    }
}

/// Build a ToolFailure for a command safety violation.
pub fn deny_message(verdict: &SafetyVerdict, tool_name: &str, raw_input: &str) -> ToolFailure {
    match verdict {
        SafetyVerdict::Allow => {
            // Should not be called with Allow, but handle gracefully.
            ToolFailure::denied("safety_violation", "Allowed (no violation).")
        }
        SafetyVerdict::Deny { reason, suggestion } => {
            let msg = format!(
                "Blocked by safety policy: {reason}\n\nTool: {tool_name}\nInput: {input}\n\nSuggestion: {suggestion}",
                input = truncate(raw_input),
            );
            ToolFailure::denied("safety_violation", &msg)
        }
    }
}

/// Build a ToolFailure for a path traversal violation.
pub fn path_deny_message(violation: &PathViolation, path: &str) -> ToolFailure {
    let msg = format!(
        "Blocked by safety policy: {violation}\n\nTool: write/edit/read\nInput: {input}\n\nSuggestion: Use a path relative to the working directory and avoid '..' components.",
        input = truncate(path),
    );
    ToolFailure::denied("safety_violation", &msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_message_format() {
        let verdict = SafetyVerdict::Deny {
            reason: "recursive delete of root directory".to_string(),
            suggestion: "restrict to a specific path".to_string(),
        };
        let tf = deny_message(&verdict, "bash", "rm -rf /");
        assert_eq!(tf.code, "safety_violation");
        assert!(
            tf.model_message
                .contains("recursive delete of root directory")
        );
        assert!(tf.model_message.contains("Tool: bash"));
        assert!(tf.model_message.contains("Input: rm -rf /"));
        assert!(
            tf.model_message
                .contains("Suggestion: restrict to a specific path")
        );
    }

    #[test]
    fn deny_message_truncates_long_input() {
        let long_cmd = "x".repeat(300);
        let verdict = SafetyVerdict::Deny {
            reason: "test".to_string(),
            suggestion: "test".to_string(),
        };
        let tf = deny_message(&verdict, "bash", &long_cmd);
        assert!(tf.model_message.contains("truncated"));
        assert!(!tf.model_message.contains(&"x".repeat(300)));
    }

    #[test]
    fn path_deny_message_format() {
        let violation = PathViolation::TraversalEscape {
            path: "../../etc/passwd".to_string(),
            resolved: std::path::PathBuf::from("/etc/passwd"),
        };
        let tf = path_deny_message(&violation, "../../etc/passwd");
        assert_eq!(tf.code, "safety_violation");
        assert!(tf.model_message.contains("escapes working directory"));
        assert!(tf.model_message.contains("Input: ../../etc/passwd"));
        assert!(tf.model_message.contains("Suggestion:"));
    }

    #[test]
    fn deny_message_multibyte_no_panic() {
        // 300 CJK chars = 900 bytes; 200th byte falls mid-character (3-byte UTF-8).
        let long_cmd = "日".repeat(300);
        let verdict = SafetyVerdict::Deny {
            reason: "test".to_string(),
            suggestion: "test".to_string(),
        };
        let tf = deny_message(&verdict, "bash", &long_cmd);
        assert!(tf.model_message.contains("truncated"));
        assert!(!tf.model_message.contains(&long_cmd));
        // Truncated portion should contain exactly MAX_RAW_INPUT_LEN chars.
        assert_eq!(
            tf.model_message
                .lines()
                .find(|l| l.starts_with("Input: "))
                .unwrap()
                .trim_start_matches("Input: ")
                .strip_suffix("... (truncated)")
                .unwrap()
                .chars()
                .count(),
            MAX_RAW_INPUT_LEN
        );
    }
}
