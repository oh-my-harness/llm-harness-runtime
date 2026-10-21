use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use crate::inbox::TaskMessageKind;

/// The frozen Orchestrator decision grammar: only the first line is
/// executable. Mirrors the controller's semantic validation so the runner
/// can seal a decision run on exactly the same inputs the controller
/// accepts. Returns the first line when it is a well-formed ACTION.
pub(crate) fn parse_action(body: &str) -> Option<&str> {
    let first = body.lines().next()?;
    if first == "ACTION: FINISH" || first == "ACTION: ESCALATE" {
        return Some(first);
    }
    let member = first.strip_prefix("ACTION: DISPATCH ")?;
    // One member id, no whitespace, and a nonempty assignment body.
    if member.is_empty()
        || member.chars().any(char::is_whitespace)
        || body.lines().skip(1).collect::<String>().trim().is_empty()
    {
        return None;
    }
    Some(first)
}

/// Return the typed terminal outcome when a task report satisfies the payload
/// invariant. ACK/PROGRESS, missing kinds, and empty terminal payloads are not
/// terminal reports.
pub fn terminal_task_report(kind: Option<TaskMessageKind>, body: &str) -> Option<TaskMessageKind> {
    kind.filter(|kind| {
        matches!(
            kind,
            TaskMessageKind::Result | TaskMessageKind::Blocked | TaskMessageKind::Error
        ) && !body.trim().is_empty()
    })
}

/// Run-local receipt set only after the exact correlated terminal message was
/// successfully delivered. AgentRunner uses it to reject plain-text endings.
#[derive(Debug, Clone)]
pub(crate) struct ControlledWorkerRun {
    task_id: String,
    reported: Arc<AtomicBool>,
    terminal_delivered: CancellationToken,
}

/// Correlated cancellation for controlled pipeline assignments. The
/// controller owns task lifetime; runners only observe the token and stop the
/// active harness run / suppress future attempts when it is cancelled.
#[derive(Debug, Default)]
pub(crate) struct ControlledTaskRegistry {
    tasks: Mutex<HashMap<String, CancellationToken>>,
    completed: Mutex<HashSet<String>>,
    replay_blocked: Mutex<HashMap<String, String>>,
}

impl ControlledTaskRegistry {
    pub(crate) fn begin(&self, task_id: &str) -> CancellationToken {
        let token = CancellationToken::new();
        self.completed.lock().remove(task_id);
        self.replay_blocked.lock().remove(task_id);
        if let Some(previous) = self.tasks.lock().insert(task_id.to_owned(), token.clone()) {
            previous.cancel();
        }
        token
    }

    pub(crate) fn block_replay(&self, task_id: &str, reason: String) {
        self.replay_blocked
            .lock()
            .insert(task_id.to_owned(), reason);
    }

    pub(crate) fn take_replay_block(&self, task_id: &str) -> Option<String> {
        self.replay_blocked.lock().remove(task_id)
    }

    pub(crate) fn token(&self, task_id: &str) -> Option<CancellationToken> {
        self.tasks.lock().get(task_id).cloned()
    }

    pub(crate) fn ensure(&self, task_id: &str) -> CancellationToken {
        self.tasks
            .lock()
            .entry(task_id.to_owned())
            .or_default()
            .clone()
    }

    pub(crate) fn is_completed(&self, task_id: &str) -> bool {
        self.completed.lock().contains(task_id)
    }

    pub(crate) fn cancel(&self, task_id: &str) -> bool {
        let token = self.tasks.lock().get(task_id).cloned();
        if let Some(token) = token {
            token.cancel();
            true
        } else {
            false
        }
    }

    pub(crate) fn complete(&self, task_id: &str) {
        self.tasks.lock().remove(task_id);
        self.retire(task_id);
    }

    pub(crate) fn retire(&self, task_id: &str) {
        self.completed.lock().insert(task_id.to_owned());
    }
}

impl ControlledWorkerRun {
    pub(crate) fn new(task_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            reported: Arc::new(AtomicBool::new(false)),
            terminal_delivered: CancellationToken::new(),
        }
    }

    pub(crate) fn record_delivery(
        &self,
        to: &str,
        task_id: Option<&str>,
        task_kind: Option<TaskMessageKind>,
        body: &str,
    ) -> bool {
        if to == "pipeline-controller"
            && task_id == Some(self.task_id.as_str())
            && terminal_task_report(task_kind, body).is_some()
            && self
                .reported
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            // send_message calls this only after durable delivery succeeds.
            // Waking the runner here therefore cannot abort before the
            // authoritative terminal report has been persisted.
            self.terminal_delivered.cancel();
            return true;
        }
        false
    }

    pub(crate) fn was_reported(&self) -> bool {
        self.reported.load(Ordering::Acquire)
    }

    pub(crate) fn task_id(&self) -> &str {
        &self.task_id
    }

    pub(crate) fn terminal_delivered(&self) -> CancellationToken {
        self.terminal_delivered.clone()
    }
}

/// Decision-local execution seal for the Orchestrator.
///
/// A valid correlated ACTION is the Orchestrator's semantic terminal. The
/// first such delivery seals the run so the harness cannot start another
/// provider turn or tool call for the same decision (real evidence: a
/// DISPATCH decision was followed by an unwanted FINISH from the same run).
/// Task lifecycle stays controller-owned; only execution is sealed here.
#[derive(Debug, Clone)]
pub(crate) struct DecisionWorkerRun {
    task_id: String,
    decided: Arc<AtomicBool>,
    /// The model attempted a decision delivery to the controller for this
    /// task (valid or malformed). A malformed attempt is nonterminal
    /// progress already visible to the controller, so the runner must not
    /// treat the run as silently missing an ACTION.
    decision_attempted: Arc<AtomicBool>,
    decision_delivered: CancellationToken,
}

impl DecisionWorkerRun {
    pub(crate) fn new(task_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            decided: Arc::new(AtomicBool::new(false)),
            decision_attempted: Arc::new(AtomicBool::new(false)),
            decision_delivered: CancellationToken::new(),
        }
    }

    pub(crate) fn record_delivery(&self, to: &str, task_id: Option<&str>, body: &str) -> bool {
        if to == "pipeline-controller" && task_id == Some(self.task_id.as_str()) {
            // Any correlated send_message attempt — including a malformed
            // ACTION the controller grammar will reject — is visible
            // progress at the controller; the decision timeout owns the
            // terminal from there.
            self.decision_attempted.store(true, Ordering::Release);
            if parse_action(body).is_some()
                && self
                    .decided
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                // Delivery is durable at this point; sealing cannot invalidate
                // the authoritative decision.
                self.decision_delivered.cancel();
                return true;
            }
        }
        false
    }

    /// True when the model ended the decision run without attempting any
    /// correlated controller delivery at all.
    pub(crate) fn was_silent(&self) -> bool {
        !self.decision_attempted.load(Ordering::Acquire)
    }

    pub(crate) fn was_decided(&self) -> bool {
        self.decided.load(Ordering::Acquire)
    }

    pub(crate) fn task_id(&self) -> &str {
        &self.task_id
    }

    pub(crate) fn decision_delivered(&self) -> CancellationToken {
        self.decision_delivered.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_worker_results_are_typed_and_require_a_body() {
        assert_eq!(
            terminal_task_report(Some(TaskMessageKind::Result), "tests passed"),
            Some(TaskMessageKind::Result)
        );
        assert_eq!(
            terminal_task_report(Some(TaskMessageKind::Blocked), "missing dependency"),
            Some(TaskMessageKind::Blocked)
        );
        assert_eq!(
            terminal_task_report(Some(TaskMessageKind::Error), "provider failed"),
            Some(TaskMessageKind::Error)
        );
        for kind in [
            None,
            Some(TaskMessageKind::Ack),
            Some(TaskMessageKind::Progress),
        ] {
            assert_eq!(terminal_task_report(kind, "working"), None, "{kind:?}");
        }
        assert_eq!(
            terminal_task_report(Some(TaskMessageKind::Result), "  "),
            None
        );
    }

    #[test]
    fn controlled_receipt_requires_exact_correlation_and_terminal_kind() {
        let receipt = ControlledWorkerRun::new("task-1");
        assert!(!receipt.record_delivery(
            "pipeline-controller",
            Some("task-1"),
            Some(TaskMessageKind::Ack),
            "accepted",
        ));
        assert!(!receipt.was_reported());
        assert!(!receipt.record_delivery(
            "pipeline-controller",
            Some("task-1"),
            Some(TaskMessageKind::Progress),
            "working",
        ));
        assert!(!receipt.was_reported());
        assert!(!receipt.record_delivery(
            "pipeline-controller",
            Some("stale"),
            Some(TaskMessageKind::Result),
            "done",
        ));
        assert!(!receipt.was_reported());
        assert!(!receipt.record_delivery(
            "operator",
            Some("task-1"),
            Some(TaskMessageKind::Result),
            "done",
        ));
        assert!(!receipt.was_reported());
        assert!(receipt.record_delivery(
            "pipeline-controller",
            Some("task-1"),
            Some(TaskMessageKind::Result),
            "done",
        ));
        assert!(receipt.was_reported());
        assert!(receipt.terminal_delivered().is_cancelled());
        assert!(!receipt.record_delivery(
            "pipeline-controller",
            Some("task-1"),
            Some(TaskMessageKind::Error),
            "late duplicate",
        ));
    }

    #[test]
    fn cancellation_is_scoped_to_one_pipeline_task_id() {
        let registry = ControlledTaskRegistry::default();
        let old = registry.begin("task-pipe-run-a-decision-1");
        let fresh = registry.begin("task-pipe-run-b-decision-1");
        assert!(registry.cancel("task-pipe-run-a-decision-1"));
        assert!(old.is_cancelled());
        assert!(!fresh.is_cancelled());
        assert!(registry.token("task-pipe-run-b-decision-1").is_some());
    }

    #[test]
    fn decision_grammar_mirrors_the_controller_action_parser() {
        assert_eq!(
            parse_action("ACTION: FINISH\nall done"),
            Some("ACTION: FINISH")
        );
        assert_eq!(parse_action("ACTION: ESCALATE"), Some("ACTION: ESCALATE"));
        assert_eq!(
            parse_action("ACTION: DISPATCH coder\nplease implement"),
            Some("ACTION: DISPATCH coder")
        );
        // Malformed shapes never seal a decision.
        assert_eq!(parse_action(""), None);
        assert_eq!(parse_action("ACTION: FINISH extra"), None);
        assert_eq!(parse_action("action: finish"), None);
        assert_eq!(parse_action("prose\nACTION: FINISH"), None);
        assert_eq!(parse_action("ACTION: DISPATCH"), None);
        assert_eq!(parse_action("ACTION: DISPATCH two words\nx"), None);
        assert_eq!(parse_action("ACTION: DISPATCH coder\n  "), None);
    }

    #[test]
    fn decision_receipt_requires_valid_first_action_and_exact_correlation() {
        let receipt = DecisionWorkerRun::new("task-pipe-d1");
        // Malformed first line must not seal the run.
        assert!(!receipt.record_delivery(
            "pipeline-controller",
            Some("task-pipe-d1"),
            "ACTION: DISPATCH coder",
        ));
        assert!(!receipt.was_decided());
        assert!(!receipt.decision_delivered().is_cancelled());
        // Wrong recipient or wrong task id must not seal either.
        assert!(!receipt.record_delivery("operator", Some("task-pipe-d1"), "ACTION: FINISH\nx"));
        assert!(!receipt.record_delivery(
            "pipeline-controller",
            Some("other"),
            "ACTION: FINISH\nx"
        ));
        // First valid correlated ACTION seals.
        assert!(receipt.record_delivery(
            "pipeline-controller",
            Some("task-pipe-d1"),
            "ACTION: DISPATCH coder\nassignment body",
        ));
        assert!(receipt.was_decided());
        assert!(receipt.decision_delivered().is_cancelled());
        // A second action can never replace the authoritative decision.
        assert!(!receipt.record_delivery(
            "pipeline-controller",
            Some("task-pipe-d1"),
            "ACTION: FINISH\nlate",
        ));
    }

    #[test]
    fn decision_silence_tracks_only_correlated_delivery_attempts() {
        // A malformed ACTION is not a decision, but a correlated delivery
        // attempt (even malformed) ends the silence: the controller has
        // visible progress and the decision timeout owns the terminal.
        let receipt = DecisionWorkerRun::new("task-pipe-d2");
        assert!(receipt.was_silent());
        // Uncorrelated deliveries never count as an attempt.
        assert!(!receipt.record_delivery("operator", Some("task-pipe-d2"), "ACTION: FINISH\nx"));
        assert!(!receipt.record_delivery(
            "pipeline-controller",
            Some("other"),
            "ACTION: FINISH\nx"
        ));
        assert!(
            receipt.was_silent(),
            "uncorrelated deliveries are not attempts"
        );
        // A malformed correlated ACTION is not a decision, but it IS an
        // attempt: the run is no longer silent.
        assert!(!receipt.record_delivery(
            "pipeline-controller",
            Some("task-pipe-d2"),
            "prose\nACTION: FINISH",
        ));
        assert!(!receipt.was_decided(), "malformed ACTION never seals");
        assert!(!receipt.was_silent(), "correlated attempt ends the silence");
        assert!(!receipt.decision_delivered().is_cancelled());
    }
}
