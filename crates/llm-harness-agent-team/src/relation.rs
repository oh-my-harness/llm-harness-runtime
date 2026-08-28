//! Relation engine: pure strength-update rules for agent relationships.

use serde::{Deserialize, Serialize};

/// Clamp a strength to `[0.0, 1.0]`, mapping non-finite values (NaN,
/// infinities) to 0.0 — they carry no meaningful strength and must not
/// poison comparisons or decay arithmetic downstream. Single source of
/// truth for every strength write path (engine math and contact store).
pub(crate) fn clamp_strength(v: f64) -> f64 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// An event that modifies the strength of a relationship between two agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelationEvent {
    /// A collaboration completed successfully — strengthens the bond.
    SuccessfulCollaboration,
    /// Routine communication (message exchange) — small strengthening.
    Communication,
    /// A collaboration failed — weakens the bond but not below zero.
    FailedCollaboration,
    /// A brand-new contact — establishes a baseline relationship.
    NewContact,
}

/// Configuration for how relationship strengths evolve.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationConfig {
    /// Strength delta applied on a successful collaboration.
    pub collaboration_success_delta: f64,
    /// Strength delta applied on routine communication.
    pub communication_delta: f64,
    /// Strength delta applied on a failed collaboration (negative).
    pub collaboration_failure_delta: f64,
    /// Starting strength assigned to a brand-new contact.
    pub initial_strength: f64,
    /// Strength decayed per elapsed tick of inactivity.
    pub decay_per_tick: f64,
    /// Strength at or below which a contact is considered forgotten.
    pub forget_threshold: f64,
}

impl Default for RelationConfig {
    fn default() -> Self {
        Self {
            collaboration_success_delta: 0.15,
            communication_delta: 0.02,
            collaboration_failure_delta: -0.05,
            initial_strength: 0.1,
            decay_per_tick: 0.001,
            forget_threshold: 0.01,
        }
    }
}

/// Pure computation engine for relationship-strength updates.
///
/// Holds no per-agent state — callers pass the current strength in and receive
/// the updated strength back. All returned values are clamped to `[0.0, 1.0]`.
pub struct RelationEngine {
    config: RelationConfig,
}

impl RelationEngine {
    /// Create a new engine with the given configuration.
    pub fn new(config: RelationConfig) -> Self {
        Self { config }
    }

    /// Borrow the engine's configuration.
    pub fn config(&self) -> &RelationConfig {
        &self.config
    }

    /// The starting strength assigned to a brand-new contact.
    ///
    /// Clamped to `[0.0, 1.0]` (non-finite → 0.0): `RelationConfig` may come
    /// from a deserialized file or a caller, so the configured value is not
    /// trusted as a valid strength.
    pub fn initial_strength(&self) -> f64 {
        clamp_strength(self.config.initial_strength)
    }

    /// Apply a relation event to a current strength value.
    ///
    /// Returns the updated strength clamped to `[0.0, 1.0]`; non-finite
    /// inputs (NaN, infinities) are treated as invalid and reset to 0.0
    /// before the delta is applied, so a poisoned value can never leak
    /// past the engine into stored contact state.
    /// `NewContact` resets to `initial_strength`.
    pub fn apply_event(&self, strength: f64, event: RelationEvent) -> f64 {
        let delta = match event {
            RelationEvent::SuccessfulCollaboration => self.config.collaboration_success_delta,
            RelationEvent::Communication => self.config.communication_delta,
            RelationEvent::FailedCollaboration => self.config.collaboration_failure_delta,
            RelationEvent::NewContact => return clamp_strength(self.config.initial_strength),
        };
        clamp_strength(clamp_strength(strength) + delta)
    }

    /// Apply time-based decay for a period of inactivity.
    ///
    /// Returns the decayed strength clamped to `[0.0, 1.0]`; non-finite
    /// inputs are reset to 0.0 (see [`Self::apply_event`]).
    pub fn apply_decay(&self, strength: f64, ticks_elapsed: u64) -> f64 {
        let decay = self.config.decay_per_tick * ticks_elapsed as f64;
        clamp_strength(clamp_strength(strength) - decay)
    }

    /// Whether a relationship has weakened enough to be forgotten.
    pub fn should_forget(&self, strength: f64) -> bool {
        strength <= self.config.forget_threshold
    }
}

impl Default for RelationEngine {
    fn default() -> Self {
        Self::new(RelationConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn successful_collaboration_increases_strength() {
        let engine = RelationEngine::default();
        let initial = 0.3;
        let updated = engine.apply_event(initial, RelationEvent::SuccessfulCollaboration);
        assert!(updated > initial, "strength should increase");
        assert!(approx(updated, 0.45), "expected 0.45, got {updated}");
    }

    #[test]
    fn communication_small_increase() {
        let engine = RelationEngine::default();
        let initial = 0.4;
        let updated = engine.apply_event(initial, RelationEvent::Communication);
        assert!(updated > initial, "strength should increase slightly");
        assert!(approx(updated, 0.42), "expected 0.42, got {updated}");
        // The increase is smaller than a successful collaboration.
        let collab = engine.apply_event(initial, RelationEvent::SuccessfulCollaboration);
        assert!(
            updated < collab,
            "communication delta < collaboration delta"
        );
    }

    #[test]
    fn failed_collaboration_small_decrease_not_zero() {
        let engine = RelationEngine::default();
        let initial = 0.5;
        let updated = engine.apply_event(initial, RelationEvent::FailedCollaboration);
        assert!(updated < initial, "strength should decrease");
        assert!(
            updated > 0.0,
            "strength should not reach zero from one failure"
        );
        assert!(approx(updated, 0.45), "expected 0.45, got {updated}");
    }

    #[test]
    fn new_contact_initial_low() {
        let engine = RelationEngine::default();
        // From any prior strength, NewContact resets to the low initial value.
        let prior = 0.9;
        let updated = engine.apply_event(prior, RelationEvent::NewContact);
        assert_eq!(updated, engine.initial_strength());
        assert!(updated < prior, "new contact should start low");
        assert!(approx(updated, 0.1), "expected 0.1, got {updated}");
    }

    #[test]
    fn non_finite_strengths_are_sanitized() {
        let engine = RelationEngine::default();
        // Non-finite input is sanitized to a 0.0 base, then the event
        // applies to that base: the engine is the last line of defense
        // before a poisoned value reaches stored contact state.
        for event in [
            RelationEvent::SuccessfulCollaboration,
            RelationEvent::Communication,
            RelationEvent::FailedCollaboration,
        ] {
            let expected = delta_of(&engine, event).clamp(0.0, 1.0);
            assert_eq!(engine.apply_event(f64::NAN, event), expected);
            assert!(engine.apply_event(f64::NAN, event).is_finite());
        }
        // Infinity likewise.
        assert!(
            engine
                .apply_event(f64::INFINITY, RelationEvent::Communication)
                .is_finite()
        );
        // Decay sanitizes the base too.
        assert!(engine.apply_decay(f64::NAN, 10).is_finite());
        assert_eq!(engine.apply_decay(f64::NAN, 10), 0.0);
    }

    /// The raw delta the engine applies for `event` (test helper).
    fn delta_of(engine: &RelationEngine, event: RelationEvent) -> f64 {
        match event {
            RelationEvent::SuccessfulCollaboration => engine.config().collaboration_success_delta,
            RelationEvent::Communication => engine.config().communication_delta,
            RelationEvent::FailedCollaboration => engine.config().collaboration_failure_delta,
            RelationEvent::NewContact => 0.0,
        }
    }
}
