//! Lifecycle model for Linux sandbox network resources.

use std::collections::HashMap;

use crate::network::{NetworkPolicy, NetworkSetup};

/// Registry state for resources managed for one sandbox network namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkResourceState {
    Active,
    Unavailable,
}

/// One immutable network identity and its current lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkResource {
    resource_id: String,
    setup: NetworkSetup,
    policy: NetworkPolicy,
    state: NetworkResourceState,
}

impl NetworkResource {
    pub fn resource_id(&self) -> &str {
        &self.resource_id
    }

    pub fn setup(&self) -> &NetworkSetup {
        &self.setup
    }

    pub fn setup_commands(&self) -> Vec<Vec<String>> {
        self.setup.setup_commands()
    }

    pub fn teardown_commands(&self) -> Vec<Vec<String>> {
        self.setup.teardown_commands()
    }

    pub fn policy(&self) -> &NetworkPolicy {
        &self.policy
    }

    pub fn state(&self) -> NetworkResourceState {
        self.state
    }
}

/// Idempotent ownership map for network resources.
///
/// The production backend will perform host operations; this model is the
/// safety boundary that prevents resource reuse before those operations land.
#[derive(Debug, Default)]
pub struct NetworkManager {
    resources: HashMap<String, NetworkResource>,
}

impl NetworkManager {
    pub fn create(&mut self, resource_id: &str, policy: NetworkPolicy) -> anyhow::Result<()> {
        if let Some(existing) = self.resources.get(resource_id) {
            anyhow::ensure!(
                existing.policy == policy,
                "network resource {resource_id:?} is already bound to another policy"
            );
            anyhow::ensure!(
                existing.state == NetworkResourceState::Active,
                "network resource {resource_id:?} is unavailable and must be cleaned up"
            );
            return Ok(());
        }

        let setup = NetworkSetup::new(resource_id)?;
        anyhow::ensure!(
            !self
                .resources
                .values()
                .any(|resource| resource.setup == setup),
            "network resource collision detected for {resource_id:?}"
        );
        self.resources.insert(
            resource_id.to_owned(),
            NetworkResource {
                resource_id: resource_id.to_owned(),
                setup,
                policy,
                state: NetworkResourceState::Active,
            },
        );
        Ok(())
    }

    pub fn get(&self, resource_id: &str) -> Option<&NetworkResource> {
        self.resources.get(resource_id)
    }

    pub fn list(&self) -> Vec<&NetworkResource> {
        self.resources.values().collect()
    }

    /// Record the result of an external host-resource probe.
    pub fn reconcile(&mut self, resource_id: &str, exists: bool) -> anyhow::Result<()> {
        let resource = self
            .resources
            .get_mut(resource_id)
            .ok_or_else(|| anyhow::anyhow!("unknown network resource {resource_id:?}"))?;
        resource.state = if exists {
            NetworkResourceState::Active
        } else {
            NetworkResourceState::Unavailable
        };
        Ok(())
    }

    pub fn delete(&mut self, resource_id: &str) -> bool {
        self.resources.remove(resource_id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::NetworkPolicy;
    use llm_harness_platform::sandbox::NetRule;

    fn policy(address: &str) -> NetworkPolicy {
        NetworkPolicy::new(
            &[NetRule {
                host: address.to_owned(),
                port_range: Some((443, 443)),
            }],
            &[],
        )
        .unwrap()
    }

    #[test]
    fn create_is_idempotent_for_the_same_policy() {
        let mut manager = NetworkManager::default();
        manager.create("sandbox-1", policy("1.1.1.1")).unwrap();
        manager.create("sandbox-1", policy("1.1.1.1")).unwrap();

        let resource = manager.get("sandbox-1").unwrap();
        assert_eq!(resource.resource_id(), "sandbox-1");
        assert_eq!(resource.state(), NetworkResourceState::Active);
        assert_eq!(resource.policy(), &policy("1.1.1.1"));
        assert_eq!(resource.setup().namespace(), "llmh-sandbox-1");
    }

    #[test]
    fn create_refuses_policy_rebinding() {
        let mut manager = NetworkManager::default();
        manager.create("sandbox-1", policy("1.1.1.1")).unwrap();
        assert!(manager.create("sandbox-1", policy("8.8.4.4")).is_err());
    }

    #[test]
    fn reconcile_quarantines_missing_resources_without_deleting_them() {
        let mut manager = NetworkManager::default();
        manager.create("sandbox-1", policy("1.1.1.1")).unwrap();
        manager.reconcile("sandbox-1", false).unwrap();

        let resource = manager.get("sandbox-1").unwrap();
        assert_eq!(resource.state(), NetworkResourceState::Unavailable);
        assert!(manager.create("sandbox-1", policy("1.1.1.1")).is_err());
    }

    #[test]
    fn delete_is_idempotent() {
        let mut manager = NetworkManager::default();
        manager.create("sandbox-1", policy("1.1.1.1")).unwrap();
        assert!(manager.delete("sandbox-1"));
        assert!(!manager.delete("sandbox-1"));
        assert!(manager.get("sandbox-1").is_none());
    }

    #[test]
    fn resource_exposes_setup_and_teardown_commands() {
        let mut manager = NetworkManager::default();
        manager.create("sandbox-1", policy("1.1.1.1")).unwrap();
        let resource = manager.get("sandbox-1").unwrap();

        assert_eq!(resource.setup_commands(), resource.setup().setup_commands());
        assert_eq!(
            resource.teardown_commands(),
            resource.setup().teardown_commands()
        );
        assert!(resource.teardown_commands().contains(&vec![
            "ip".to_owned(),
            "netns".to_owned(),
            "delete".to_owned(),
            "llmh-sandbox-1".to_owned(),
        ]));
    }

    #[test]
    fn list_returns_all_resources() {
        let mut manager = NetworkManager::default();
        manager.create("sandbox-1", policy("1.1.1.1")).unwrap();
        manager.create("sandbox-2", policy("8.8.4.4")).unwrap();

        let resources = manager.list();
        let mut resource_ids = resources
            .iter()
            .map(|resource| resource.resource_id())
            .collect::<Vec<_>>();
        resource_ids.sort();

        assert_eq!(resource_ids, vec!["sandbox-1", "sandbox-2"]);
    }
}
