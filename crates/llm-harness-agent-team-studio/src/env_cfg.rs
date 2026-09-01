//! Provider wiring shared by the HTTP layer and the binary.
//!
//! The panel can register projects at runtime, so the HTTP layer needs a
//! way to build providers without re-reading the config file. The binary
//! installs a [`ProviderFactory`] (from its config); tests install a mock
//! factory. Model tiers come from `CT_MODELS` ("strong=x,main=y,cheap=z"),
//! set once at startup; tests set it directly.

use crate::roles::ModelTier;
use parking_lot::RwLock;
use std::sync::Arc;

/// Builds the LLM client list for a newly registered project. The binary
/// provides the real factory (config-driven); tests provide mocks.
pub type ProviderFactory =
    Arc<dyn Fn() -> Vec<(String, Arc<dyn llm_harness_loop::LlmClient>)> + Send + Sync>;

static PROVIDER_FACTORY: RwLock<Option<ProviderFactory>> = RwLock::new(None);

/// Install the process-wide provider factory (called once by the binary,
/// or by a test). Later calls overwrite earlier ones.
pub fn install_provider_factory(f: ProviderFactory) {
    *PROVIDER_FACTORY.write() = Some(f);
}

#[cfg(test)]
pub struct TestProviderFactoryGuard {
    _lock: parking_lot::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for TestProviderFactoryGuard {
    fn drop(&mut self) {
        *PROVIDER_FACTORY.write() = None;
    }
}

/// Serialize tests that install the process-wide factory while retaining the
/// factory for the lifetime of the returned guard.
#[cfg(test)]
#[must_use]
pub fn install_test_provider_factory(f: ProviderFactory) -> TestProviderFactoryGuard {
    static TEST_FACTORY_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    let lock = TEST_FACTORY_LOCK.lock();
    *PROVIDER_FACTORY.write() = Some(f);
    TestProviderFactoryGuard { _lock: lock }
}

/// Invoke the installed factory; empty vec when none installed (routes
/// reject the registration with a clear error in that case).
pub fn providers_from_factory() -> Vec<(String, Arc<dyn llm_harness_loop::LlmClient>)> {
    PROVIDER_FACTORY
        .read()
        .as_ref()
        .map(|f| f())
        .unwrap_or_default()
}

/// Parse `CT_MODELS` (format: `strong=x,main=y,cheap=z`). Missing pairs
/// default to the `main` value; fully missing variable defaults all tiers
/// to `"default"` (mock-model territory).
pub fn tier_from_env() -> ModelTier {
    let raw = std::env::var("CT_MODELS").unwrap_or_default();
    if raw.trim().is_empty() {
        return ModelTier {
            strong: "default".into(),
            main: "default".into(),
            cheap: "default".into(),
        };
    }
    let mut tier = ModelTier {
        strong: String::new(),
        main: String::new(),
        cheap: String::new(),
    };
    for pair in raw.split(',') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        match k.trim() {
            "strong" => tier.strong = v.trim().to_string(),
            "main" => tier.main = v.trim().to_string(),
            "cheap" => tier.cheap = v.trim().to_string(),
            _ => {}
        }
    }
    let fallback = if tier.main.is_empty() {
        "default".to_string()
    } else {
        tier.main.clone()
    };
    for slot in [&mut tier.strong, &mut tier.main, &mut tier.cheap] {
        if slot.is_empty() {
            *slot = fallback.clone();
        }
    }
    tier
}

/// Scout patrol interval from `CT_SCOUT_INTERVAL` (0 = disabled).
pub fn scout_interval_from_env() -> u64 {
    std::env::var("CT_SCOUT_INTERVAL")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(3600)
}
