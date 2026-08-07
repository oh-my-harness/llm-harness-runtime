//! Loop safety plugin — guards against pathological agent loops.
//!
//! This module hosts five loop-safety hooks (items 15–19):
//! - **Repetition guard** (15): detects repeated tool calls with identical args.
//! - **Failure circuit breaker** (16): trips after N consecutive failures of the
//!   same tool.
//! - **Truncation continuation** (17): auto-continues truncated responses up to N
//!   times using a continuation prompt.
//! - **Death spiral guard** (18): stops the loop after N consecutive error turns.
//! - **Max turns** (19): hard cap on total turn count.
//!
//! `mod.rs` provides the configuration types and the `LoopSafetyPlugin`
//! entry point. All five hooks are registered by `register_hooks`.

pub(crate) mod death_spiral;
pub(crate) mod failure_breaker;
pub(crate) mod max_turns;
pub(crate) mod repetition;
pub(crate) mod state;
pub(crate) mod truncation;

use death_spiral::DeathSpiralGuardHook;
use failure_breaker::FailureCircuitBreakerHook;
use max_turns::MaxTurnsHook;
use repetition::RepetitionGuardHook;
pub(crate) use state::LoopSafetyState;
use truncation::TruncationContinuationHook;

use std::sync::Arc;

use llm_harness_agent::{HarnessHooks, Plugin};
use parking_lot::Mutex;

// ── Sub-configs ───────────────────────────────────────────────────────────────

/// Item 15 — repetition guard: denies a tool call whose argument fingerprint
/// has appeared `max_repetitions` consecutive times.
#[derive(Clone, Debug)]
pub struct RepetitionGuardConfig {
    /// Max consecutive identical calls before denial.
    pub max_repetitions: u32,
    /// Message returned to the model on denial.
    pub deny_message: String,
}

impl Default for RepetitionGuardConfig {
    fn default() -> Self {
        Self {
            max_repetitions: 3,
            deny_message: "repetition detected".to_string(),
        }
    }
}

/// Item 16 — failure circuit breaker: denies calls to a tool after it fails
/// `max_consecutive_failures` times in a row.
#[derive(Clone, Debug)]
pub struct FailureCircuitBreakerConfig {
    /// Max consecutive failures before the circuit trips.
    pub max_consecutive_failures: u32,
    /// Message returned to the model when the circuit is open.
    pub deny_message: String,
}

impl Default for FailureCircuitBreakerConfig {
    fn default() -> Self {
        Self {
            max_consecutive_failures: 3,
            deny_message: "circuit broken".to_string(),
        }
    }
}

/// Item 17 — truncation continuation: when the model's response is truncated,
/// automatically issue a continuation prompt up to `max_continuations` times.
#[derive(Clone, Debug)]
pub struct TruncationContinuationConfig {
    /// Max auto-continuations per run.
    pub max_continuations: u32,
    /// Continuation prompt; `{text}` is replaced with the truncated tail.
    pub continuation_prompt: String,
}

impl Default for TruncationContinuationConfig {
    fn default() -> Self {
        Self {
            max_continuations: 2,
            continuation_prompt:
                "请从上次输出的断点继续生成，不要重复已输出的内容。上次输出的最后一段：\n{text}"
                    .to_string(),
        }
    }
}

/// Item 18 — death spiral guard: stops the loop after `max_error_turns`
/// consecutive turns that produced any tool failure.
#[derive(Clone, Debug)]
pub struct DeathSpiralGuardConfig {
    /// Max consecutive error turns before the loop is halted.
    pub max_error_turns: u32,
}

impl Default for DeathSpiralGuardConfig {
    fn default() -> Self {
        Self { max_error_turns: 5 }
    }
}

/// Item 19 — max turns: hard cap on the total number of turns in a run.
#[derive(Clone, Debug)]
pub struct MaxTurnsConfig {
    /// Absolute maximum turns; the loop stops once this is reached.
    pub max_turns: u32,
}

impl Default for MaxTurnsConfig {
    fn default() -> Self {
        Self { max_turns: 50 }
    }
}

// ── Aggregate config ──────────────────────────────────────────────────────────

/// Aggregate configuration for [`LoopSafetyPlugin`].
///
/// Each field is an `Option`: `Some` enables that guard with the given config,
/// `None` disables it. [`LoopSafetyConfig::default_enabled`] turns everything
/// on with defaults; [`LoopSafetyConfig::disabled`] turns everything off.
#[derive(Clone, Debug, Default)]
pub struct LoopSafetyConfig {
    /// Item 15 — repetition guard. `None` = disabled.
    pub repetition_guard: Option<RepetitionGuardConfig>,
    /// Item 16 — failure circuit breaker. `None` = disabled.
    pub failure_circuit_breaker: Option<FailureCircuitBreakerConfig>,
    /// Item 17 — truncation continuation. `None` = disabled.
    pub truncation_continuation: Option<TruncationContinuationConfig>,
    /// Item 18 — death spiral guard. `None` = disabled.
    pub death_spiral_guard: Option<DeathSpiralGuardConfig>,
    /// Item 19 — max turns. `None` = disabled.
    pub max_turns: Option<MaxTurnsConfig>,
}

impl LoopSafetyConfig {
    /// Enable every guard with its default configuration.
    pub fn default_enabled() -> Self {
        Self {
            repetition_guard: Some(RepetitionGuardConfig::default()),
            failure_circuit_breaker: Some(FailureCircuitBreakerConfig::default()),
            truncation_continuation: Some(TruncationContinuationConfig::default()),
            death_spiral_guard: Some(DeathSpiralGuardConfig::default()),
            max_turns: Some(MaxTurnsConfig::default()),
        }
    }

    /// Disable every guard (no-op plugin).
    pub fn disabled() -> Self {
        Self::default()
    }
}

// ── Plugin ────────────────────────────────────────────────────────────────────

/// Loop safety plugin: bundles the five loop-safety hooks behind a single
/// configuration surface.
///
/// Shared mutable state lives in `Arc<Mutex<LoopSafetyState>>` so that all
/// hooks installed by this plugin observe the same counters.
pub struct LoopSafetyPlugin {
    /// Per-guard configuration; `None` fields disable the corresponding hook.
    pub config: LoopSafetyConfig,
    pub(crate) state: Arc<Mutex<LoopSafetyState>>,
}

impl LoopSafetyPlugin {
    /// Create a plugin with the given config; state is fresh.
    pub fn new(config: LoopSafetyConfig) -> Self {
        Self {
            config,
            state: Arc::new(Mutex::new(LoopSafetyState::default())),
        }
    }
}

impl Plugin for LoopSafetyPlugin {
    fn name(&self) -> &str {
        "loop-safety"
    }
    fn register_hooks(&self, hooks: &mut HarnessHooks) {
        if let Some(cfg) = &self.config.repetition_guard {
            let hook = Arc::new(RepetitionGuardHook::new(self.state.clone(), cfg.clone()));
            hooks.before_tool_call.push(hook.clone());
            hooks.should_stop.push(hook);
        }
        if let Some(cfg) = &self.config.failure_circuit_breaker {
            let hook = Arc::new(FailureCircuitBreakerHook::new(
                self.state.clone(),
                cfg.clone(),
            ));
            hooks.before_tool_call.push(hook.clone());
            hooks.after_tool_call.push(hook.clone());
            hooks.should_stop.push(hook);
        }
        if let Some(cfg) = &self.config.truncation_continuation {
            let hook = Arc::new(TruncationContinuationHook::new(
                self.state.clone(),
                cfg.clone(),
            ));
            hooks.should_stop.push(hook.clone());
            hooks.transform_context.push(hook);
        }
        if let Some(cfg) = &self.config.death_spiral_guard {
            let hook = Arc::new(DeathSpiralGuardHook::new(self.state.clone(), cfg.clone()));
            hooks.prepare_next_turn.push(hook.clone());
            hooks.should_stop.push(hook);
        }
        if let Some(cfg) = &self.config.max_turns {
            let hook = Arc::new(MaxTurnsHook::new(cfg.clone()));
            hooks.should_stop.push(hook.clone());
            hooks.prepare_next_turn.push(hook);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_enables_all() {
        let cfg = LoopSafetyConfig::default_enabled();
        assert!(cfg.repetition_guard.is_some());
        assert!(cfg.failure_circuit_breaker.is_some());
        assert!(cfg.truncation_continuation.is_some());
        assert!(cfg.death_spiral_guard.is_some());
        assert!(cfg.max_turns.is_some());

        // Spot-check a couple of defaults to guard against silent drift.
        assert_eq!(
            cfg.repetition_guard.as_ref().unwrap().max_repetitions,
            RepetitionGuardConfig::default().max_repetitions
        );
        assert_eq!(
            cfg.max_turns.as_ref().unwrap().max_turns,
            MaxTurnsConfig::default().max_turns
        );
    }

    #[test]
    fn disabled_config_registers_nothing() {
        let plugin = LoopSafetyPlugin::new(LoopSafetyConfig::disabled());
        let mut hooks = HarnessHooks::none();
        plugin.register_hooks(&mut hooks);
        // All hooks disabled — every hook vector must be empty.
        assert!(hooks.before_tool_call.is_empty());
        assert!(hooks.after_tool_call.is_empty());
        assert!(hooks.should_stop.is_empty());
        assert!(hooks.transform_context.is_empty());
        assert!(hooks.prepare_next_turn.is_empty());
    }

    #[test]
    fn plugin_name() {
        let plugin = LoopSafetyPlugin::new(LoopSafetyConfig::default_enabled());
        assert_eq!(plugin.name(), "loop-safety");
    }

    #[test]
    fn default_enabled_registers_all_hooks() {
        let plugin = LoopSafetyPlugin::new(LoopSafetyConfig::default_enabled());
        let mut hooks = HarnessHooks::none();
        plugin.register_hooks(&mut hooks);
        // repetition_guard: before_tool_call + should_stop
        // failure_circuit_breaker: before_tool_call + after_tool_call + should_stop
        // truncation_continuation: should_stop + transform_context
        // death_spiral_guard: prepare_next_turn + should_stop
        // max_turns: should_stop + prepare_next_turn
        assert_eq!(
            hooks.before_tool_call.len(),
            2,
            "repetition + failure_breaker"
        );
        assert_eq!(hooks.after_tool_call.len(), 1, "failure_breaker only");
        assert_eq!(hooks.should_stop.len(), 5, "all 5 hooks");
        assert_eq!(hooks.transform_context.len(), 1, "truncation only");
        assert_eq!(hooks.prepare_next_turn.len(), 2, "death_spiral + max_turns");
    }
}
