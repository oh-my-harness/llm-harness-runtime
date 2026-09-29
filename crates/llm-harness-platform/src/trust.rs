//! Trust profile: how much host access a sandbox session is granted.

use serde::{Deserialize, Serialize};

/// Trust level granted to a sandbox session.
///
/// Ordering matters: `Restricted < TrustedLan < TrustedFull`. Principals
/// declare a maximum profile they are willing to grant; requests above the
/// cap are rejected.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustProfile {
    /// No network, work_dir + explicit allowlist only. Third-party code and
    /// sensitive-data workloads.
    #[default]
    Restricted,
    /// Network allowed; host project directories bound read-write; system
    /// areas and credential floors still denied.
    #[serde(alias = "trusted-lan")]
    TrustedLan,
    /// Network allowed; near-host filesystem access (system areas and
    /// credential floors still denied).
    #[serde(alias = "trusted-full")]
    TrustedFull,
}
impl TrustProfile {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrustProfile::Restricted => "restricted",
            TrustProfile::TrustedLan => "trusted-lan",
            TrustProfile::TrustedFull => "trusted-full",
        }
    }
}
