//! Shared hook contract and transport-independent protocol types.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Environment variable containing the supervisor-assigned hook server name.
pub const HARNX_HOOK_NAME: &str = "HARNX_HOOK_NAME";
/// JetStream KV bucket containing hook server registrations.
pub const HOOK_REGISTRY_BUCKET: &str = "harnx_hook_registry";
/// JetStream KV bucket containing hooks local supervisors require to be available.
pub const HOOK_EXPECTATIONS_BUCKET: &str = "harnx_hook_expectations";
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

impl FailPolicy {
    /// Lowercase value accepted by hook-server command-line arguments.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Open => "open",
        }
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_label: Option<String>,
    pub hooks: Vec<HookSpec>,
    pub schema_version: u32,
    pub proto_version: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_registration_round_trips_through_serde() -> anyhow::Result<()> {
        #[derive(Deserialize)]
        struct LegacyHookRegistration {
            server: String,
            hooks: Vec<HookSpec>,
            schema_version: u32,
            proto_version: u32,
        }

        let mut registration = HookRegistration {
            server: "test-hooks".to_string(),
            display_label: None,
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
        assert!(!encoded
            .windows(b"display_label".len())
            .any(|window| window == b"display_label"));
        let decoded: HookRegistration = serde_json::from_slice(&encoded)?;
        assert_eq!(decoded.display_label, None);
        assert_eq!(decoded, registration);

        registration.display_label = Some("Friendly hook".to_string());
        let encoded = serde_json::to_vec(&registration)?;
        let legacy: LegacyHookRegistration = serde_json::from_slice(&encoded)?;
        assert_eq!(legacy.server, registration.server);
        assert_eq!(legacy.hooks, registration.hooks);
        assert_eq!(legacy.schema_version, registration.schema_version);
        assert_eq!(legacy.proto_version, registration.proto_version);

        assert_eq!(FailPolicy::default(), FailPolicy::Closed);
        assert_eq!(FailPolicy::Closed.as_str(), "closed");
        assert_eq!(FailPolicy::Open.as_str(), "open");
        Ok(())
    }
}
