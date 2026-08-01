//! Shared hook contract and transport-independent protocol types.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// JetStream KV bucket containing hook server registrations.
pub const HOOK_REGISTRY_BUCKET: &str = "harnx_hook_registry";
/// Current hook registration schema version.
pub const HOOK_SCHEMA_VERSION: u32 = 1;
/// Current hook request protocol version.
pub const HOOK_PROTOCOL_VERSION: u32 = 1;

/// A hook event subscription advertised by a hook server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookSpec {
    /// Hook event name, such as `PreToolUse` or `PostToolUse`.
    pub event: String,
    /// Optional regular expression matched against the bare tool name.
    pub matcher: Option<String>,
    /// Dispatch priority. Lower values run first.
    pub priority: i32,
    /// Optional execution timeout in seconds.
    pub timeout_secs: Option<u64>,
    /// Behavior to apply when hook execution fails.
    pub fail_policy: FailPolicy,
}

/// Behavior to apply when a hook can't produce an outcome.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailPolicy {
    /// Reject the operation when the hook fails.
    #[default]
    Closed,
    /// Continue the operation when the hook fails.
    Open,
}

/// Hook implementation served by a transport adapter.
#[async_trait]
pub trait Hook: Send + Sync {
    /// Stable server name used in registration and transport subjects.
    fn name(&self) -> &str;

    /// Hook event subscriptions exposed by this server.
    fn hooks(&self) -> Vec<HookSpec>;

    /// Handles one hook event.
    async fn handle_hook(
        &self,
        payload: harnx_core::hooks::HookPayload,
    ) -> harnx_core::hooks::HookOutcome;
}

/// Hook server metadata stored in KV for discovery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookRegistration {
    pub server: String,
    pub hooks: Vec<HookSpec>,
    pub schema_version: u32,
    pub proto_version: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_registration_round_trips_through_serde() -> anyhow::Result<()> {
        let registration = HookRegistration {
            server: "test-hooks".to_string(),
            hooks: vec![HookSpec {
                event: "PreToolUse".to_string(),
                matcher: Some("exec".to_string()),
                priority: 10,
                timeout_secs: Some(5),
                fail_policy: FailPolicy::default(),
            }],
            schema_version: HOOK_SCHEMA_VERSION,
            proto_version: HOOK_PROTOCOL_VERSION,
        };

        let encoded = serde_json::to_vec(&registration)?;
        let decoded: HookRegistration = serde_json::from_slice(&encoded)?;

        assert_eq!(decoded, registration);
        assert_eq!(FailPolicy::default(), FailPolicy::Closed);
        Ok(())
    }
}
