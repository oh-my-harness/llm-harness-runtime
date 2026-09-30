//! Linux network namespace policy modeling for sandbox egress.

use std::hash::{Hash, Hasher};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use anyhow::anyhow;
use llm_harness_platform::sandbox::NetRule;

const MAX_PORT: u16 = 65535;

/// A validated global egress destination and destination-port range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NetworkRule {
    target: IpAddr,
    prefix: Option<u8>,
    ports: (u16, u16),
}

impl NetworkRule {
    pub fn target(&self) -> IpAddr {
        self.target
    }

    pub fn prefix(&self) -> Option<u8> {
        self.prefix
    }

    pub fn ports(&self) -> (u16, u16) {
        self.ports
    }
}

/// A fail-closed outbound policy for one network namespace.
///
/// Hostnames are deliberately rejected here. The gateway must resolve and
/// authorize the returned address set before constructing policy; otherwise a
/// DNS response could turn a hostname rule into unrestricted egress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkPolicy {
    rules: Vec<NetworkRule>,
    dns_servers: Vec<IpAddr>,
}

impl NetworkPolicy {
    pub fn new(rules: &[NetRule], dns_servers: &[IpAddr]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !rules.is_empty(),
            "empty network allowlists must use Bwrap network isolation, not a network namespace"
        );

        let mut validated_rules = Vec::with_capacity(rules.len());
        for rule in rules {
            let host = rule.host.trim();
            anyhow::ensure!(!host.is_empty(), "network allowlist host cannot be empty");
            let (target, prefix) = parse_ip_or_cidr(host)?;
            validate_global_ip(&target)?;
            if let Some(prefix) = prefix {
                let maximum = if target.is_ipv4() { 32 } else { 128 };
                anyhow::ensure!(
                    prefix < maximum,
                    "single-address CIDR {host:?} must use an IP literal"
                );
            } else {
                anyhow::ensure!(
                    !matches!(target, IpAddr::V4(Ipv4Addr::LOCALHOST))
                        && !matches!(target, IpAddr::V6(address) if address.is_loopback()),
                    "loopback network allowlists are not supported"
                );
                anyhow::ensure!(
                    !is_private_ip(&target),
                    "private network allowlists are not supported from sandboxes"
                );
            }

            let ports = match rule.port_range {
                Some((start, end)) if start > 0 && start <= end => (start, end),
                Some((start, end)) => anyhow::bail!(
                    "invalid network port range {start}-{end}; ports must be within 1-65535"
                ),
                None => (1, MAX_PORT),
            };
            validated_rules.push(NetworkRule {
                target,
                prefix,
                ports,
            });
        }

        let mut validated_dns = Vec::with_capacity(dns_servers.len());
        for address in dns_servers {
            validate_global_ip(address)?;
            if !validated_dns.contains(address) {
                validated_dns.push(*address);
            }
        }

        Ok(Self {
            rules: validated_rules,
            dns_servers: validated_dns,
        })
    }

    pub fn rules(&self) -> &[NetworkRule] {
        &self.rules
    }

    pub fn dns_servers(&self) -> &[IpAddr] {
        &self.dns_servers
    }
}

/// Deterministic host resources backing one sandbox network namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkSetup {
    namespace: String,
    veth: String,
    host_ip: Ipv4Addr,
    guest_ip: Ipv4Addr,
}

impl NetworkSetup {
    pub fn new(resource_id: &str) -> anyhow::Result<Self> {
        validate_resource_id(resource_id)?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        resource_id.hash(&mut hasher);
        let hash = hasher.finish() as u32;
        let host_suffix = ((hash & 0x03) << 2) as u8;
        Ok(Self {
            namespace: format!("llmh-{resource_id}"),
            veth: format!("v{hash:014x}"),
            host_ip: Ipv4Addr::new(10, 200, (hash >> 8) as u8, host_suffix + 1),
            guest_ip: Ipv4Addr::new(10, 200, (hash >> 8) as u8, host_suffix + 2),
        })
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn veth(&self) -> &str {
        &self.veth
    }

    pub fn host_ip(&self) -> Ipv4Addr {
        self.host_ip
    }

    pub fn guest_ip(&self) -> Ipv4Addr {
        self.guest_ip
    }

    pub fn setup_commands(&self) -> Vec<Vec<String>> {
        vec![
            vec![
                "ip".into(),
                "netns".into(),
                "add".into(),
                self.namespace.clone(),
            ],
            vec![
                "ip".into(),
                "link".into(),
                "add".into(),
                self.veth.clone(),
                "type".into(),
                "veth".into(),
                "peer".into(),
                "name".into(),
                "eth0".into(),
            ],
            vec![
                "ip".into(),
                "link".into(),
                "set".into(),
                "eth0".into(),
                "netns".into(),
                self.namespace.clone(),
            ],
            vec![
                "ip".into(),
                "address".into(),
                "add".into(),
                format!("{}/30", self.host_ip),
                "dev".into(),
                self.veth.clone(),
            ],
            vec![
                "ip".into(),
                "link".into(),
                "set".into(),
                self.veth.clone(),
                "up".into(),
            ],
            vec![
                "ip".into(),
                "netns".into(),
                "exec".into(),
                self.namespace.clone(),
                "ip".into(),
                "link".into(),
                "set".into(),
                "lo".into(),
                "up".into(),
            ],
            vec![
                "ip".into(),
                "netns".into(),
                "exec".into(),
                self.namespace.clone(),
                "ip".into(),
                "address".into(),
                "add".into(),
                format!("{}/30", self.guest_ip),
                "dev".into(),
                "eth0".into(),
            ],
            vec![
                "ip".into(),
                "netns".into(),
                "exec".into(),
                self.namespace.clone(),
                "ip".into(),
                "link".into(),
                "set".into(),
                "eth0".into(),
                "up".into(),
            ],
            vec![
                "ip".into(),
                "netns".into(),
                "exec".into(),
                self.namespace.clone(),
                "ip".into(),
                "route".into(),
                "replace".into(),
                "default".into(),
                "via".into(),
                self.host_ip.to_string(),
            ],
        ]
    }
}

fn validate_resource_id(resource_id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        (1..=48).contains(&resource_id.len()),
        "network resource id length must be between 1 and 48"
    );
    anyhow::ensure!(
        resource_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-'),
        "network resource id may contain only ASCII letters, digits, and hyphens"
    );
    anyhow::ensure!(
        resource_id
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric()),
        "network resource id must start with a letter or digit"
    );
    Ok(())
}

fn parse_ip_or_cidr(host: &str) -> anyhow::Result<(IpAddr, Option<u8>)> {
    if let Some((address, prefix)) = host.rsplit_once('/') {
        let address = address
            .parse::<IpAddr>()
            .map_err(|error| anyhow!("invalid network CIDR {host:?}: {error}"))?;
        let maximum = if address.is_ipv4() { 32 } else { 128 };
        let prefix = prefix
            .parse::<u8>()
            .map_err(|error| anyhow!("invalid network CIDR prefix {host:?}: {error}"))?;
        anyhow::ensure!(prefix <= maximum, "invalid network CIDR prefix {host:?}");
        Ok((address, Some(prefix)))
    } else {
        let address = host
            .parse::<IpAddr>()
            .map_err(|_| anyhow!("network allowlist host {host:?} must be a global IP or CIDR"))?;
        Ok((address, None))
    }
}

fn validate_global_ip(address: &IpAddr) -> anyhow::Result<()> {
    let valid = match address {
        IpAddr::V4(address) => {
            !(address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_multicast()
                || address.is_unspecified()
                || address.is_broadcast())
        }
        IpAddr::V6(address) => {
            !(address.is_loopback()
                || address.is_multicast()
                || address.is_unspecified()
                || is_unicast_link_local(address)
                || is_unique_local(address))
        }
    };
    anyhow::ensure!(
        valid,
        "network allowlist target {address} must be globally routable"
    );
    Ok(())
}

fn is_private_ip(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_private() || address.is_loopback(),
        IpAddr::V6(address) => address.is_loopback() || is_unique_local(address),
    }
}

fn is_unique_local(address: &Ipv6Addr) -> bool {
    address.segments()[0] & 0xfe00 == 0xfc00
}

fn is_unicast_link_local(address: &Ipv6Addr) -> bool {
    address.segments()[0] & 0xffc0 == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(host: &str, ports: Option<(u16, u16)>) -> NetRule {
        NetRule {
            host: host.to_string(),
            port_range: ports,
        }
    }

    #[test]
    fn accepts_global_ips_and_cidrs() {
        let policy = NetworkPolicy::new(
            &[
                rule("203.0.113.10", Some((443, 443))),
                rule("2001:db8:10::/48", None),
            ],
            &[],
        )
        .unwrap();

        assert_eq!(policy.rules().len(), 2);
        assert_eq!(policy.rules()[0].target().to_string(), "203.0.113.10");
        assert_eq!(policy.rules()[0].ports(), (443, 443));
        assert_eq!(policy.rules()[1].prefix(), Some(48));
    }

    #[test]
    fn rejects_unsafe_targets_and_ports() {
        for rule in [
            rule("api.example.com", None),
            rule("192.168.1.10", None),
            rule("127.0.0.1", None),
            rule("203.0.113.10", Some((0, 443))),
            rule("203.0.113.10", Some((443, 80))),
        ] {
            assert!(
                NetworkPolicy::new(&[rule], &[]).is_err(),
                "unsafe network rule must be rejected"
            );
        }
    }

    #[test]
    fn rejects_empty_allowlist_and_private_dns() {
        assert!(NetworkPolicy::new(&[], &[]).is_err());
        assert!(
            NetworkPolicy::new(
                &[rule("203.0.113.10", None)],
                &["192.168.0.53".parse().unwrap()]
            )
            .is_err()
        );
    }

    #[test]
    fn setup_determines_safe_resource_names() {
        let setup = NetworkSetup::new("0197d6f8-8d5c-7c7f-8dc4-4ba50b0f84f7").unwrap();
        let other = NetworkSetup::new("0197d6f8-8d5c-7c7f-8dc4-4ba50b0f84f8").unwrap();

        assert!(setup.namespace().starts_with("llmh-"));
        assert!(setup.veth().len() <= 15);
        assert_ne!(setup.veth(), other.veth());
        assert_eq!(
            setup.guest_ip().octets()[3],
            setup.host_ip().octets()[3] + 1
        );
    }

    #[test]
    fn rejects_path_like_resource_ids() {
        assert!(NetworkSetup::new("../escape").is_err());
        assert!(NetworkSetup::new("").is_err());
        assert!(NetworkSetup::new(&"a".repeat(49)).is_err());
    }

    #[test]
    fn setup_builds_deterministic_network_commands() {
        let setup = NetworkSetup::new("sandbox-1").unwrap();
        let commands = setup.setup_commands();
        assert!(commands.contains(&vec![
            "ip".to_string(),
            "netns".to_string(),
            "add".to_string(),
            setup.namespace().to_string(),
        ]));
        assert!(commands.contains(&vec![
            "ip".to_string(),
            "netns".to_string(),
            "exec".to_string(),
            setup.namespace().to_string(),
            "ip".to_string(),
            "route".to_string(),
            "replace".to_string(),
            "default".to_string(),
            "via".to_string(),
            setup.host_ip().to_string(),
        ]));
    }
}
