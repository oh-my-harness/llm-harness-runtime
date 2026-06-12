// AuthHook and AuthInfo come directly from core — runtime does not redefine them.
//
// Core definition (llm_harness_types::hooks):
//   pub trait AuthHook: Send + Sync {
//       fn resolve<'a>(&'a self) -> BoxFuture<'a, Result<AuthInfo, AgentError>>;
//   }
//   pub struct AuthInfo {
//       pub api_key: Option<String>,
//       pub headers: Vec<(String, String)>,
//   }
//
// Business layer implements AuthHook by doing: impl llm_harness_types::AuthHook for MyAuth { ... }
pub use llm_harness_types::{AuthHook, AuthInfo};
