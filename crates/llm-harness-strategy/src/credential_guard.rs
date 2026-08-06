//! MCP credential scope guard hook.
//!
//! `McpCredentialGuardHook` implements `BeforeToolCallHook`. It checks whether
//! the current tool call is authorized under the MCP server's credential
//! scope declarations. By default it only logs a warning for unauthorized
//! tools; a `deny_unauthorized` flag can be set to `true` for fail-closed
//! mode.
//!
//! Due to MCP's process-level credential model (env/headers are shared across
//! all tools on a server), this hook provides detection, not enforcement.
//! True per-tool credential isolation would require separate transport
//! connections per tool, which is impractical.

use futures::FutureExt;
use futures::future::BoxFuture;
use llm_harness_types::{
    BeforeToolCallCtx, BeforeToolCallDecision, BeforeToolCallHook, ToolFailure,
};

// Re-export credential lookup types from llm-harness-types so callers
// can import them from either crate.
pub use llm_harness_types::{CredentialLookup, CredentialLookupFn, build_credential_lookup};

/// BeforeToolCallHook that warns (or denies) when a tool call is not
/// authorized under its server's credential scope declarations.
///
/// - `deny_unauthorized = false` (default): logs a warning, allows the call.
/// - `deny_unauthorized = true`: denies the call with a credential scope
///   violation error.
pub struct McpCredentialGuardHook {
    lookup: CredentialLookupFn,
    deny_unauthorized: bool,
}

impl McpCredentialGuardHook {
    /// Create a warning-only guard. Unauthorized tool calls are logged but allowed.
    pub fn warning(lookup: CredentialLookupFn) -> Self {
        Self {
            lookup,
            deny_unauthorized: false,
        }
    }

    /// Create a fail-closed guard. Unauthorized tool calls are denied.
    pub fn denying(lookup: CredentialLookupFn) -> Self {
        Self {
            lookup,
            deny_unauthorized: true,
        }
    }
}

impl BeforeToolCallHook for McpCredentialGuardHook {
    fn on_call<'a>(&'a self, ctx: BeforeToolCallCtx<'a>) -> BoxFuture<'a, BeforeToolCallDecision> {
        let result = (self.lookup)(ctx.tool_name);
        async move {
            match result {
                llm_harness_types::CredentialLookup::NotScoped
                | llm_harness_types::CredentialLookup::Authorized(_) => {
                    BeforeToolCallDecision::Allow
                }
                llm_harness_types::CredentialLookup::Unauthorized => {
                    if self.deny_unauthorized {
                        tracing::warn!(
                            tool = ctx.tool_name,
                            "denying MCP tool call: credential scope violation"
                        );
                        BeforeToolCallDecision::Deny(ToolFailure::new(
                            "credential_scope_violation",
                            "Tool call denied: this tool is not in any credential's allowed_tools list.",
                        ))
                    } else {
                        tracing::warn!(
                            tool = ctx.tool_name,
                            "MCP tool call not authorized by any credential scope (allowed with warning)"
                        );
                        BeforeToolCallDecision::Allow
                    }
                }
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // NOTE: `BeforeToolCallCtx` requires `&RunContext` and `&AssistantMessage`,
    // neither of which implements `Default`. Constructing them in a unit test
    // is impractical, so we test the lookup logic directly instead of driving
    // the full `BeforeToolCallHook::on_call` path.

    #[test]
    fn build_lookup_authorizes_correctly() {
        let mut authorized = HashMap::new();
        authorized.insert("mcp__srv_tool_a".to_owned(), vec!["TOKEN".to_owned()]);
        let mut unauthorized = std::collections::HashSet::new();
        unauthorized.insert("mcp__srv_tool_b".to_owned());
        let lookup = build_credential_lookup(authorized, unauthorized);

        match lookup("mcp__srv_tool_a") {
            CredentialLookup::Authorized(creds) => assert_eq!(creds, vec!["TOKEN"]),
            _ => panic!("expected Authorized"),
        }
        match lookup("mcp__srv_tool_b") {
            CredentialLookup::Unauthorized => {}
            _ => panic!("expected Unauthorized"),
        }
        match lookup("other") {
            CredentialLookup::NotScoped => {}
            _ => panic!("expected NotScoped"),
        }
    }

    #[test]
    fn build_lookup_treats_empty_creds_as_unauthorized() {
        let mut authorized = HashMap::new();
        authorized.insert("mcp__srv_tool_c".to_owned(), vec![]);
        let unauthorized = std::collections::HashSet::new();
        let lookup = build_credential_lookup(authorized, unauthorized);

        match lookup("mcp__srv_tool_c") {
            CredentialLookup::Unauthorized => {}
            _ => panic!("expected Unauthorized for empty creds"),
        }
    }
}
