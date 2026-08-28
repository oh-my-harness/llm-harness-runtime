//! Contact list: per-agent registry of known peers with strength scores.

use std::collections::HashMap;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::AgentId;
use crate::error::Result;
use crate::relation::clamp_strength;

/// A record of a known peer agent and the strength of the relationship.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContactRecord {
    /// Id of the peer agent this record describes.
    pub peer_agent_id: AgentId,
    /// One-line description of the peer's specialty, used by
    /// `search_contacts` matching.
    pub peer_expertise: String,
    /// Relationship strength in `[0.0, 1.0]`; drives contact eviction and
    /// search ranking.
    pub strength: f64,
    /// Value of the owning agent's tick at the last interaction with this
    /// peer; the gap to the current tick drives decay.
    pub last_interaction_tick: u64,
}

/// A bounded collection of contacts for a single agent.
///
/// Each agent has its own `tick` counter that advances on every communication
/// action. The tick drives per-agent relation-strength decay: contacts not
/// interacted with recently decay based on the gap between the agent's current
/// tick and the contact's `last_interaction_tick`.
pub struct ContactIndex {
    contacts: HashMap<AgentId, ContactRecord>,
    max_contacts: usize,
    tick: u64,
}

impl ContactIndex {
    /// Create an empty index with the given contact capacity.
    pub fn new(max_contacts: usize) -> Self {
        Self {
            contacts: HashMap::new(),
            max_contacts,
            tick: 0,
        }
    }

    /// Add a new contact. If the list is at capacity, evicts the weakest
    /// contact (ties broken by least-recent interaction) to make room —
    /// but only if the incoming record is at least as strong as the
    /// weakest. A full list with an even-weaker newcomer returns
    /// `Err(ContactListFull)`. Capacity 0 rejects everything.
    pub fn add(&mut self, record: ContactRecord) -> Result<()> {
        if self.contacts.contains_key(&record.peer_agent_id) {
            self.contacts.insert(record.peer_agent_id.clone(), record);
            return Ok(());
        }
        if self.contacts.len() >= self.max_contacts {
            // Find the weakest contact; ties break toward the least-recently
            // interacted (smaller tick) so eviction is deterministic (LRU).
            let weakest = self
                .contacts
                .iter()
                .min_by(|a, b| {
                    a.1.strength
                        .partial_cmp(&b.1.strength)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(a.1.last_interaction_tick.cmp(&b.1.last_interaction_tick))
                })
                .map(|(k, v)| (k.clone(), v.strength));
            let Some((weakest_id, weakest_strength)) = weakest else {
                // Empty index (max_contacts == 0): no one to evict.
                return Err(crate::error::TeamError::ContactListFull(self.max_contacts));
            };
            // A new contact weaker than the weakest incumbent adds no
            // value — reject instead of churning the list.
            if record.strength < weakest_strength {
                return Err(crate::error::TeamError::ContactListFull(self.max_contacts));
            }
            self.contacts.remove(&weakest_id);
        }
        self.contacts.insert(record.peer_agent_id.clone(), record);
        Ok(())
    }

    /// Look up a contact by peer id.
    pub fn find(&self, peer: &AgentId) -> Option<&ContactRecord> {
        self.contacts.get(peer)
    }

    /// Mutable lookup of a contact by peer id.
    pub fn find_mut(&mut self, peer: &AgentId) -> Option<&mut ContactRecord> {
        self.contacts.get_mut(peer)
    }

    /// Remove a contact, returning the removed record if it existed.
    pub fn remove(&mut self, peer: &AgentId) -> Option<ContactRecord> {
        self.contacts.remove(peer)
    }

    /// Number of contacts in the index.
    pub fn len(&self) -> usize {
        self.contacts.len()
    }

    /// Whether the index holds no contacts.
    pub fn is_empty(&self) -> bool {
        self.contacts.is_empty()
    }

    /// Return contacts whose strength is below `threshold`.
    pub fn below_threshold(&self, threshold: f64) -> Vec<ContactRecord> {
        self.contacts
            .values()
            .filter(|c| c.strength < threshold)
            .cloned()
            .collect()
    }

    /// Advance this agent's tick by one and return the new value.
    pub fn advance_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// The agent's current tick value.
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// Apply decay without resetting `last_interaction_tick`.
    ///
    /// Reduces each contact's strength by `decay_per_tick * tick_gap` via
    /// the relation engine (the single decay implementation), using
    /// `self.tick`, but does NOT update `last_interaction_tick`. This is
    /// used before a relation event to apply accumulated decay while
    /// preserving each contact's last-interaction timestamp.
    pub fn apply_decay_preserve(&mut self, engine: &crate::relation::RelationEngine) {
        for c in self.contacts.values_mut() {
            let tick_gap = self.tick.saturating_sub(c.last_interaction_tick);
            if tick_gap > 0 {
                // Route through the engine's apply_decay — the single
                // decay implementation (clamp + non-finite sanitization).
                c.strength = engine.apply_decay(c.strength, tick_gap);
            }
        }
    }

    /// Return all contacts sorted by strength (descending); ties break
    /// toward the most recent interaction. Deterministic order — consumers
    /// (search_contacts truncation, future UI) see the strongest contacts
    /// first regardless of HashMap iteration order.
    pub fn list_contacts(&self) -> Vec<ContactRecord> {
        let mut out: Vec<ContactRecord> = self.contacts.values().cloned().collect();
        out.sort_by(|a, b| {
            b.strength
                .partial_cmp(&a.strength)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.last_interaction_tick.cmp(&a.last_interaction_tick))
        });
        out
    }
}

/// Per-agent contact store, safe for concurrent access.
pub struct ContactStore {
    indices: Mutex<HashMap<AgentId, ContactIndex>>,
}

impl Default for ContactStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ContactStore {
    /// Create an empty store with no per-agent indices.
    pub fn new() -> Self {
        Self {
            indices: Mutex::new(HashMap::new()),
        }
    }

    /// Add a contact for `agent`, creating the index if needed.
    pub fn add_contact(
        &self,
        agent: &AgentId,
        record: ContactRecord,
        max_contacts: usize,
    ) -> Result<()> {
        let mut indices = self.indices.lock();
        let index = indices
            .entry(agent.clone())
            .or_insert_with(|| ContactIndex::new(max_contacts));
        index.add(record)
    }

    /// List all contacts of an agent.
    pub fn list_contacts(&self, agent: &AgentId) -> Vec<ContactRecord> {
        let indices = self.indices.lock();
        match indices.get(agent) {
            Some(idx) => idx.list_contacts(),
            None => Vec::new(),
        }
    }

    /// Check whether `peer` is a known contact of `agent`.
    pub fn has_contact(&self, agent: &AgentId, peer: &AgentId) -> bool {
        let indices = self.indices.lock();
        indices
            .get(agent)
            .is_some_and(|idx| idx.find(peer).is_some())
    }

    /// Get a copy of a contact record, if it exists.
    pub fn get_contact(&self, agent: &AgentId, peer: &AgentId) -> Option<ContactRecord> {
        let indices = self.indices.lock();
        indices.get(agent).and_then(|idx| idx.find(peer).cloned())
    }

    /// Get the agent's current tick value (0 if the agent has no contact
    /// index yet).
    pub fn current_tick(&self, agent: &AgentId) -> u64 {
        let indices = self.indices.lock();
        indices.get(agent).map_or(0, |idx| idx.tick())
    }

    /// Update the strength of a specific contact.
    ///
    /// The value is clamped to `[0.0, 1.0]`; non-finite values (NaN) are
    /// treated as invalid and reset to 0.0. This mirrors the
    /// RelationEngine's clamping guarantee on the store-side write path.
    pub fn update_strength(&self, agent: &AgentId, peer: &AgentId, strength: f64) {
        let mut indices = self.indices.lock();
        if let Some(idx) = indices.get_mut(agent)
            && let Some(c) = idx.find_mut(peer)
        {
            c.strength = clamp_strength(strength);
        }
    }

    /// Advance the agent's tick, ensure the peer exists as a contact, apply
    /// accumulated decay to all contacts, then update the peer's
    /// `last_interaction_tick` and apply the relation event.
    ///
    /// After the decay pass, contacts whose strength has fallen to the
    /// engine's forget threshold are removed — long-idle relationships are
    /// forgotten rather than lingering forever (complementary to capacity
    /// eviction, which handles space pressure).
    pub fn apply_relation_event(
        &self,
        agent: &AgentId,
        peer: &AgentId,
        peer_expertise: &str,
        event: crate::relation::RelationEvent,
        engine: &crate::relation::RelationEngine,
        max_contacts: usize,
    ) {
        let mut indices = self.indices.lock();
        let idx = indices
            .entry(agent.clone())
            .or_insert_with(|| ContactIndex::new(max_contacts));
        // Advance this agent's own tick.
        let tick = idx.advance_tick();
        // Ensure the peer exists as a contact at the current tick.
        if !idx.contacts.contains_key(peer) {
            let record = ContactRecord {
                peer_agent_id: peer.clone(),
                peer_expertise: peer_expertise.to_string(),
                strength: clamp_strength(engine.initial_strength()),
                last_interaction_tick: tick,
            };
            let _ = idx.add(record);
        }
        // Apply accumulated decay to all contacts (preserves timestamps),
        // then forget anyone who has fallen to the forget threshold —
        // long-idle relationships are dropped, not kept as dead weight.
        // The peer just contacted is never forgotten in the same pass.
        idx.apply_decay_preserve(engine);
        let threshold = engine.config().forget_threshold;
        let to_forget: Vec<AgentId> = idx
            .below_threshold(threshold)
            .into_iter()
            .map(|c| c.peer_agent_id)
            .filter(|id| id != peer)
            .collect();
        for id in to_forget {
            idx.remove(&id);
        }
        // Update the contacted peer's tick and apply the event.
        if let Some(c) = idx.find_mut(peer) {
            c.last_interaction_tick = tick;
            c.strength = engine.apply_event(c.strength, event);
        }
    }

    /// Remove contacts below the given threshold.
    pub fn forget_below_threshold(&self, agent: &AgentId, threshold: f64) {
        let mut indices = self.indices.lock();
        if let Some(idx) = indices.get_mut(agent) {
            let to_remove: Vec<AgentId> = idx
                .below_threshold(threshold)
                .into_iter()
                .map(|c| c.peer_agent_id)
                .collect();
            for id in to_remove {
                idx.remove(&id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::TeamError;

    fn record(peer: &str, expertise: &str, strength: f64, tick: u64) -> ContactRecord {
        ContactRecord {
            peer_agent_id: AgentId::new(peer),
            peer_expertise: expertise.to_string(),
            strength,
            last_interaction_tick: tick,
        }
    }

    #[test]
    fn list_contacts_returns_all() {
        let mut idx = ContactIndex::new(10);
        let _ = idx.add(record("alpha", "rust systems programming", 0.8, 0));
        let _ = idx.add(record("beta", "rust web frontend", 0.5, 0));
        let _ = idx.add(record("gamma", "python data science", 0.9, 0));

        let results = idx.list_contacts();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn add_evicts_weakest_when_full() {
        let mut idx = ContactIndex::new(2);
        assert!(idx.add(record("a", "x", 0.8, 0)).is_ok());
        assert!(idx.add(record("b", "x", 0.3, 0)).is_ok());
        // Full — adding "c" should evict "b" (weakest, strength 0.3).
        assert!(idx.add(record("c", "x", 0.5, 0)).is_ok());
        assert_eq!(idx.len(), 2);
        assert!(idx.find(&AgentId::new("a")).is_some());
        assert!(idx.find(&AgentId::new("b")).is_none());
        assert!(idx.find(&AgentId::new("c")).is_some());
        // Re-adding existing contact is fine (update).
        assert!(idx.add(record("a", "updated", 0.5, 0)).is_ok());
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn apply_decay_preserve_reduces_strength() {
        let mut idx = ContactIndex::new(10);
        let _ = idx.add(record("a", "rust", 0.5, 100));
        // Advance tick to 200 to create a 100-tick gap.
        for _ in 0..200 {
            idx.advance_tick();
        }
        idx.apply_decay_preserve(&crate::relation::RelationEngine::default());
        // tick_gap = 200 - 100 = 100; decay = 0.001 * 100 = 0.1
        // new strength = 0.5 - 0.1 = 0.4
        let rec = idx.find(&AgentId::new("a")).unwrap();
        assert!((rec.strength - 0.4).abs() < 1e-9);
        // last_interaction_tick is preserved (not reset).
        assert_eq!(rec.last_interaction_tick, 100);
    }

    #[test]
    fn below_threshold_returns_low_strength_contacts() {
        let mut idx = ContactIndex::new(10);
        let _ = idx.add(record("a", "rust", 0.9, 0));
        let _ = idx.add(record("b", "rust", 0.05, 0));
        let _ = idx.add(record("c", "rust", 0.3, 0));

        let low = idx.below_threshold(0.1);
        assert_eq!(low.len(), 1);
        assert_eq!(low[0].peer_agent_id.as_str(), "b");
    }

    #[test]
    fn add_rejects_weaker_newcomer_when_full() {
        let mut idx = ContactIndex::new(2);
        assert!(idx.add(record("a", "x", 0.8, 0)).is_ok());
        assert!(idx.add(record("b", "x", 0.3, 0)).is_ok());
        // Full, newcomer weaker than the weakest incumbent → rejected.
        let err = idx.add(record("c", "x", 0.2, 0)).unwrap_err();
        assert!(matches!(err, TeamError::ContactListFull(2)));
        assert_eq!(idx.len(), 2);
        assert!(idx.find(&AgentId::new("c")).is_none());
    }

    #[test]
    fn add_rejects_everything_at_zero_capacity() {
        let mut idx = ContactIndex::new(0);
        let err = idx.add(record("a", "x", 1.0, 0)).unwrap_err();
        assert!(matches!(err, TeamError::ContactListFull(0)));
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn add_eviction_tiebreaks_by_lru() {
        let mut idx = ContactIndex::new(2);
        // Same strength — eviction must pick the older interaction (tick 5),
        // deterministically, regardless of HashMap iteration order.
        assert!(idx.add(record("old", "x", 0.5, 5)).is_ok());
        assert!(idx.add(record("recent", "x", 0.5, 50)).is_ok());
        assert!(idx.add(record("c", "x", 0.5, 100)).is_ok());
        assert!(
            idx.find(&AgentId::new("old")).is_none(),
            "LRU entry evicted"
        );
        assert!(idx.find(&AgentId::new("recent")).is_some());
        assert!(idx.find(&AgentId::new("c")).is_some());
    }

    #[test]
    fn update_strength_clamps_and_sanitizes() {
        let store = ContactStore::new();
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        store
            .add_contact(&alice, record("bob", "x", 0.5, 0), 10)
            .unwrap();

        // Out-of-range values are clamped.
        store.update_strength(&alice, &bob, 5.0);
        assert_eq!(store.get_contact(&alice, &bob).unwrap().strength, 1.0);
        store.update_strength(&alice, &bob, -3.0);
        assert_eq!(store.get_contact(&alice, &bob).unwrap().strength, 0.0);
        // NaN is treated as invalid → 0.0, not propagated.
        store.update_strength(&alice, &bob, f64::NAN);
        assert_eq!(store.get_contact(&alice, &bob).unwrap().strength, 0.0);
    }

    #[test]
    fn decay_never_produces_nan_from_sanitized_state() {
        let mut idx = ContactIndex::new(10);
        // A NaN strength injected directly (simulating a legacy bad write)
        // must not survive decay: clamp maps non-finite to 0.0.
        idx.add(record("bad", "x", f64::NAN, 0)).unwrap();
        idx.advance_tick();
        idx.apply_decay_preserve(&crate::relation::RelationEngine::default());
        let rec = idx.find(&AgentId::new("bad")).unwrap();
        assert_eq!(rec.strength, 0.0);
    }

    #[test]
    fn list_contacts_sorted_by_strength_desc() {
        let mut idx = ContactIndex::new(10);
        idx.add(record("mid", "x", 0.5, 10)).unwrap();
        idx.add(record("low", "x", 0.1, 20)).unwrap();
        idx.add(record("high", "x", 0.9, 30)).unwrap();
        // Same strength — LRU tiebreak: higher tick first.
        idx.add(record("tie_old", "x", 0.5, 5)).unwrap();
        idx.add(record("tie_new", "x", 0.5, 99)).unwrap();

        let list = idx.list_contacts();
        let ids: Vec<&str> = list.iter().map(|c| c.peer_agent_id.as_str()).collect();
        assert_eq!(ids, vec!["high", "tie_new", "mid", "tie_old", "low"]);
    }

    #[test]
    fn apply_relation_event_new_contact_clamps_initial_strength() {
        // The ensure-new-contact record path must not write the engine's raw
        // configured initial strength: an out-of-range RelationConfig (e.g.
        // 1.5 from a deserialized config) must land in [0, 1] and finite.
        let engine = crate::relation::RelationEngine::new(crate::relation::RelationConfig {
            initial_strength: 1.5,
            ..Default::default()
        });
        let store = ContactStore::new();
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");

        store.apply_relation_event(
            &alice,
            &bob,
            "python",
            crate::relation::RelationEvent::NewContact,
            &engine,
            10,
        );

        let rec = store.get_contact(&alice, &bob).unwrap();
        assert!(
            rec.strength.is_finite() && (0.0..=1.0).contains(&rec.strength),
            "stored strength {} must be in [0, 1] and finite",
            rec.strength
        );
        assert_eq!(rec.strength, 1.0);
    }

    #[test]
    fn apply_relation_event_forgets_decayed_contacts() {
        let engine = crate::relation::RelationEngine::default();
        let store = ContactStore::new();
        let alice = AgentId::new("alice");
        let stale = AgentId::new("stale");
        let bob = AgentId::new("bob");

        // "stale" starts weak and never interacts again.
        store
            .add_contact(&alice, record("stale", "x", 0.005, 0), 10)
            .unwrap();

        // Every interaction advances alice's tick; "stale" decays further
        // and eventually falls to/below the forget threshold (0.01).
        for _ in 0..20 {
            store.apply_relation_event(
                &alice,
                &bob,
                "y",
                crate::relation::RelationEvent::Communication,
                &engine,
                10,
            );
        }
        assert!(
            store.get_contact(&alice, &stale).is_none(),
            "decayed-to-threshold contact must be forgotten"
        );
        // The actively-contacted peer is never forgotten.
        assert!(store.get_contact(&alice, &bob).is_some());
    }

    #[test]
    fn forget_threshold_not_applied_to_active_peer() {
        let engine = crate::relation::RelationEngine::default();
        let store = ContactStore::new();
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        // Bob starts below the forget threshold but is the interaction
        // target — the forget pass must skip the peer it is about to
        // strengthen, and the Communication event must lift him out.
        store
            .add_contact(&alice, record("bob", "x", 0.001, 0), 10)
            .unwrap();
        store.apply_relation_event(
            &alice,
            &bob,
            "x",
            crate::relation::RelationEvent::Communication,
            &engine,
            10,
        );
        // Not forgotten + strengthened by the communication event
        // (0.0 after decay clamp, + communication_delta 0.02).
        let rec = store.get_contact(&alice, &bob).unwrap();
        assert!((rec.strength - 0.02).abs() < 1e-9);
    }
}
