//! Durable per-run execution limits and immutable run/invocation identity.
//!
//! Policy snapshots for nonrenewing autonomous deadlines. Admission and worker
//! timer wiring are separate from these resolver/storage contracts. Records are:
//!
//! - **Immutable**: Once written, never modified by later runs
//! - **Durable**: Storage APIs require create-only writes; admission wiring must persist before execution
//! - **Fenced**: Bound to specific run/invocation identities
//!
//! # Key Types
//!
//! - [`RunIdentity`]: UUID v7 for the root run, minted at trusted external admission
//! - [`InvocationIdentity`]: UUID for one admission into one conversation
//! - [`RunLimitsRecord`]: Immutable admission record with deadline
//! - [`EffectiveDeadline`]: Computed deadline after override + ancestor clamp
//!
//! # Semantics
//!
//! - Omitted/null/nonpositive timeout inherits from target/global policy
//! - Unconfigured policy has a finite 86400-second (24-hour) fallback
//! - Positive timeout overrides local allowance, then ancestor deadline clamps
//! - Replay/config reload cannot extend active deadlines

use chrono::{DateTime, Utc};
use harnx_core::{agent_config::AgentConfig, config_data::RunLimitsConfig};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Unique identifier for a root run, minted once at trusted external admission.
///
/// A RunIdentity is immutable and never overwritten by later independent runs
/// in the same conversation. Each distinct external instruction that starts
/// autonomous work gets a new RunIdentity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunIdentity(String);

impl RunIdentity {
    /// Create a new run identity using UUID v7 (time-ordered).
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }

    /// Create from an existing string (for loading persisted records).
    pub fn from_string(s: String) -> Self {
        Self(s)
    }

    /// Get the string representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for RunIdentity {
    fn default() -> Self {
        Self::new()
    }
}

/// Unique identifier for one invocation within a run.
///
/// Each admission into a conversation gets a fresh InvocationIdentity.
/// Retries/replay of the same logical call keep the same identity.
/// New child calls, handoffs, and resumed prompts get new identities
/// but inherit the RunIdentity and its deadline.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InvocationIdentity(String);

impl InvocationIdentity {
    /// Create a new invocation identity using UUID v4.
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Create from an existing string (for loading persisted records).
    pub fn from_string(s: String) -> Self {
        Self(s)
    }

    /// Get the string representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for InvocationIdentity {
    fn default() -> Self {
        Self::new()
    }
}

/// Edge kind from parent to child invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationEdgeKind {
    /// Direct delegation from parent to child tool call.
    Delegation,
    /// Handoff transferring execution to another target.
    Handoff,
    /// Macro step or continuation within the same scope.
    MacroContinuation,
}

/// Immutable record of run limits at admission time.
///
/// This record is persisted before execution and never modified.
/// It captures the original admission timestamp, resolved deadline,
/// and provenance for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLimitsRecord {
    /// Unique identifier for this root run.
    pub run_id: RunIdentity,
    /// Unique identifier for this specific invocation.
    pub invocation_id: InvocationIdentity,
    /// UTC timestamp when this invocation was admitted.
    pub admitted_at: DateTime<Utc>,
    /// Absolute deadline frozen at admission. Policy resolution always supplies one.
    pub deadline: Option<DateTime<Utc>>,
    /// Source of the policy (for diagnostics).
    pub policy_source: RunLimitsPolicySource,
    /// Link to parent invocation, if any.
    pub parent_invocation: Option<ParentInvocationLink>,
}

impl RunLimitsRecord {
    /// Resolve a root admission once. Persist this record before starting work.
    pub fn admit_root(
        run_id: RunIdentity,
        invocation_id: InvocationIdentity,
        admitted_at: DateTime<Utc>,
        global: RunLimitsConfig,
        target: Option<&AgentConfig>,
        call: CallTimeoutOverride,
    ) -> Result<Self, RunLimitsError> {
        let effective = EffectiveDeadline::resolve(global, target, call, None, admitted_at)?;
        Ok(Self {
            run_id,
            invocation_id,
            admitted_at,
            deadline: effective.deadline,
            policy_source: effective.source,
            parent_invocation: None,
        })
    }

    /// Resolve a child using its parent's frozen effective deadline, not current config.
    pub fn admit_child(
        parent: &RunLimitsRecord,
        invocation_id: InvocationIdentity,
        edge_kind: InvocationEdgeKind,
        admitted_at: DateTime<Utc>,
        global: RunLimitsConfig,
        target: Option<&AgentConfig>,
        call: CallTimeoutOverride,
    ) -> Result<Self, RunLimitsError> {
        let effective =
            EffectiveDeadline::resolve(global, target, call, Some(parent), admitted_at)?;
        Ok(Self {
            run_id: parent.run_id.clone(),
            invocation_id,
            admitted_at,
            deadline: effective.deadline,
            policy_source: effective.source,
            parent_invocation: Some(ParentInvocationLink {
                invocation_id: parent.invocation_id.clone(),
                edge_kind,
            }),
        })
    }

    /// Check if this record's deadline has passed at the given time.
    pub fn is_expired_at(&self, now: DateTime<Utc>) -> bool {
        match self.deadline {
            Some(deadline) => now >= deadline,
            None => false,
        }
    }

    /// Get remaining time until deadline, if any.
    pub fn remaining(&self, now: DateTime<Utc>) -> Option<Duration> {
        self.deadline.and_then(|deadline| {
            if deadline > now {
                deadline.signed_duration_since(now).to_std().ok()
            } else {
                None
            }
        })
    }
}

/// Link to the parent invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentInvocationLink {
    pub invocation_id: InvocationIdentity,
    pub edge_kind: InvocationEdgeKind,
}

/// Source of the run limits policy (for diagnostics).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunLimitsPolicySource {
    /// Global configuration default.
    GlobalDefault,
    /// Target agent's configuration.
    TargetAgent { agent_name: String },
    /// Explicit override in the call.
    ExplicitOverride,
    /// Inherited from parent invocation.
    InheritedFrom { parent_invocation_id: String },
}

/// Computed effective deadline after applying resolution logic.
///
/// This combines the target configuration with any override and
/// ancestor deadline propagation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveDeadline {
    /// Resolved finite deadline, in the same representation as persisted records.
    pub deadline: Option<DateTime<Utc>>,
    /// Whether the local override was applied.
    pub local_override_applied: bool,
    /// Whether an ancestor deadline clamped the effective value.
    pub ancestor_clamped: bool,
    /// Source information for diagnostics.
    pub source: RunLimitsPolicySource,
}

impl EffectiveDeadline {
    /// Resolve global/target policy, call intent, and frozen ancestor in one place.
    /// `target` must be the effective configuration loaded by the target worker,
    /// including package patches. No caller-supplied diagnostic source is accepted.
    pub fn resolve(
        global: RunLimitsConfig,
        target: Option<&AgentConfig>,
        call: CallTimeoutOverride,
        parent: Option<&RunLimitsRecord>,
        admitted_at: DateTime<Utc>,
    ) -> Result<Self, RunLimitsError> {
        let target_override = target
            .and_then(|agent| agent.run_limits())
            .and_then(|limits| limits.timeout_secs);
        let configured = global.resolve(target_override.as_ref()).timeout_secs;
        let (timeout, mut source) = match call {
            CallTimeoutOverride::Omitted => {
                let source = match (target, target_override) {
                    (Some(agent), Some(timeout)) if !timeout.is_omit() => {
                        RunLimitsPolicySource::TargetAgent {
                            agent_name: agent.name().to_owned(),
                        }
                    }
                    _ => RunLimitsPolicySource::GlobalDefault,
                };
                (configured, source)
            }
            CallTimeoutOverride::Finite(secs) => (secs, RunLimitsPolicySource::ExplicitOverride),
        };
        let local = compute_deadline(admitted_at, timeout.get())?;
        // Equal deadlines retain the local source; the ancestor did not shorten it.
        let ancestor_clamped = parent
            .and_then(|p| p.deadline)
            .is_some_and(|ancestor| ancestor < local);
        let deadline = if ancestor_clamped {
            let parent = parent.expect("ancestor requires parent");
            source = RunLimitsPolicySource::InheritedFrom {
                parent_invocation_id: parent.invocation_id.as_str().to_owned(),
            };
            parent.deadline
        } else {
            Some(local)
        };
        Ok(Self {
            deadline,
            local_override_applied: !matches!(call, CallTimeoutOverride::Omitted),
            ancestor_clamped,
            source,
        })
    }
}

/// Per-call positive allowance, or inheritance from target/global finite policy.
/// Nonpositive model arguments are normalized to omission before durable admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallTimeoutOverride {
    /// No explicit override - use target policy.
    Omitted,
    /// Explicit positive timeout.
    Finite(std::num::NonZeroU64),
}

impl CallTimeoutOverride {
    /// Zero and omission both inherit; signed arguments are normalized by the parser.
    pub fn from_optional(secs: Option<u64>) -> Self {
        match secs {
            None | Some(0) => Self::Omitted,
            Some(n) => Self::Finite(std::num::NonZeroU64::new(n).expect("n > 0")),
        }
    }
}

/// Compute a deadline from admission time and timeout seconds.
///
/// Uses checked arithmetic to prevent overflow. The timeout_secs must fit
/// into the signed i64 range for Duration conversion.
fn compute_deadline(
    admitted_at: DateTime<Utc>,
    timeout_secs: u64,
) -> Result<DateTime<Utc>, RunLimitsError> {
    // Check that timeout_secs fits in i64 (max ~292 billion years)
    let timeout_i64 = i64::try_from(timeout_secs)
        .map_err(|_| RunLimitsError::InvalidTimeoutSeconds(timeout_secs))?;

    // Convert to Duration safely
    let duration = chrono::Duration::try_seconds(timeout_i64)
        .ok_or(RunLimitsError::InvalidTimeoutSeconds(timeout_secs))?;

    // Add to admission time
    admitted_at
        .checked_add_signed(duration)
        .ok_or(RunLimitsError::DeadlineOverflow {
            admitted_at,
            timeout_secs,
        })
}

/// Dispatch stopped before starting new work. The worker must append a fenced
/// Cancel, not a second Error or a forced final model response.
#[derive(Debug)]
pub struct DeadlineExpired;
impl std::fmt::Display for DeadlineExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("autonomous invocation deadline expired")
    }
}
impl std::error::Error for DeadlineExpired {}

impl DeadlineExpired {
    /// Keep the typed stop for worker handling while exposing useful gate failures
    /// through tool error envelopes, which otherwise only carry a string.
    pub(crate) fn before_dispatch(record: &RunLimitsRecord) -> anyhow::Error {
        let timeout =
            crate::TimeoutTerminal::from_record(record).expect("expired record has a deadline");
        let scope = serde_json::to_value(timeout.scope).expect("timeout scope JSON");
        anyhow::Error::new(Self).context(format!(
            "Invocation deadline expired before dispatch (scope: {}, deadline: {}, run_id: {}, invocation_id: {}). Do not retry in this expired invocation. Return to the user to confirm continuation with a new external instruction. Inspect available saved public results. No new dispatch occurred; tool output from this attempt is unavailable.",
            scope.as_str().expect("timeout scope string"), timeout.deadline, timeout.run_id, timeout.invocation_id
        ))
    }
}

/// Errors in run limits computation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunLimitsError {
    /// timeout_secs value too large to compute deadline.
    InvalidTimeoutSeconds(u64),

    /// admitted_at + timeout_secs exceeds DateTime limit.
    DeadlineOverflow {
        admitted_at: DateTime<Utc>,
        timeout_secs: u64,
    },
}

impl std::fmt::Display for RunLimitsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTimeoutSeconds(secs) => {
                write!(
                    f,
                    "timeout_secs value {secs} is too large to compute deadline"
                )
            }
            Self::DeadlineOverflow {
                admitted_at,
                timeout_secs,
            } => {
                write!(
                    f,
                    "deadline overflow: admitted_at {admitted_at} + {timeout_secs}s exceeds DateTime limit"
                )
            }
        }
    }
}

impl std::error::Error for RunLimitsError {}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::config_data::RunLimitsTimeout;
    use std::num::NonZeroU64;

    fn time() -> DateTime<Utc> {
        "2026-01-01T00:00:00Z".parse().unwrap()
    }
    fn finite(secs: u64) -> RunLimitsConfig {
        RunLimitsConfig {
            timeout_secs: RunLimitsTimeout::Finite(NonZeroU64::new(secs).unwrap()),
        }
    }
    fn root(secs: u64) -> RunLimitsRecord {
        RunLimitsRecord::admit_root(
            RunIdentity::new(),
            InvocationIdentity::new(),
            time(),
            finite(secs),
            None,
            CallTimeoutOverride::Omitted,
        )
        .unwrap()
    }

    #[test]
    fn resolution_derives_global_target_call_and_ancestor_sources() {
        let target = AgentConfig::from_markdown(
            "pkg/target",
            "---\nrun_limits:\n  timeout_secs: 40\n---\nTarget",
        )
        .unwrap();
        let parent = root(20);
        let resolve = |call, parent| {
            EffectiveDeadline::resolve(finite(80), Some(&target), call, parent, time()).unwrap()
        };
        let configured = resolve(CallTimeoutOverride::Omitted, None);
        assert_eq!(
            configured.deadline,
            Some(time() + chrono::Duration::seconds(40))
        );
        assert_eq!(
            configured.source,
            RunLimitsPolicySource::TargetAgent {
                agent_name: "pkg/target".into()
            }
        );
        let overridden = resolve(CallTimeoutOverride::from_optional(Some(10)), Some(&parent));
        assert_eq!(overridden.source, RunLimitsPolicySource::ExplicitOverride);
        assert!(!overridden.ancestor_clamped);
        for call in [
            CallTimeoutOverride::from_optional(Some(0)),
            CallTimeoutOverride::from_optional(Some(200)),
        ] {
            let inherited = resolve(call, Some(&parent));
            assert_eq!(inherited.deadline, parent.deadline);
            assert!(inherited.ancestor_clamped);
            assert_eq!(
                inherited.source,
                RunLimitsPolicySource::InheritedFrom {
                    parent_invocation_id: parent.invocation_id.as_str().into()
                }
            );
        }
        let global = EffectiveDeadline::resolve(
            finite(80),
            None,
            CallTimeoutOverride::Omitted,
            None,
            time(),
        )
        .unwrap();
        assert_eq!(global.source, RunLimitsPolicySource::GlobalDefault);
        let zero = resolve(CallTimeoutOverride::from_optional(Some(0)), None);
        assert_eq!(zero, configured);
        assert!(!zero.local_override_applied);
    }

    #[test]
    fn default_is_finite_24_hours_and_null_call_inherits() {
        let parsed: Option<u64> = serde_json::from_str("null").unwrap();
        assert_eq!(
            CallTimeoutOverride::from_optional(parsed),
            CallTimeoutOverride::Omitted
        );
        let result = EffectiveDeadline::resolve(
            RunLimitsConfig::default(),
            None,
            CallTimeoutOverride::from_optional(parsed),
            None,
            time(),
        )
        .unwrap();
        assert_eq!(
            result.deadline,
            Some(time() + chrono::Duration::seconds(86400))
        );
        let result = EffectiveDeadline::resolve(
            finite(60),
            None,
            CallTimeoutOverride::from_optional(parsed),
            None,
            time(),
        )
        .unwrap();
        assert_eq!(
            result.deadline,
            Some(time() + chrono::Duration::seconds(60))
        );
    }

    #[test]
    fn late_child_and_grandchild_cannot_escape_short_ancestor() {
        let parent = root(100);
        let child = RunLimitsRecord::admit_child(
            &parent,
            InvocationIdentity::new(),
            InvocationEdgeKind::Delegation,
            time() + chrono::Duration::seconds(90),
            finite(60),
            None,
            CallTimeoutOverride::Omitted,
        )
        .unwrap();
        assert_eq!(child.run_id, parent.run_id);
        assert_eq!(child.deadline, parent.deadline);
        let grandchild = RunLimitsRecord::admit_child(
            &child,
            InvocationIdentity::new(),
            InvocationEdgeKind::Handoff,
            time() + chrono::Duration::seconds(110),
            RunLimitsConfig::default(),
            None,
            CallTimeoutOverride::from_optional(Some(0)),
        )
        .unwrap();
        assert_eq!(grandchild.deadline, parent.deadline);
        assert!(grandchild.is_expired_at(grandchild.admitted_at));
        assert_eq!(
            grandchild.policy_source,
            RunLimitsPolicySource::InheritedFrom {
                parent_invocation_id: child.invocation_id.as_str().into()
            }
        );
    }

    #[test]
    fn equal_deadline_retains_local_source_and_longer_parent_does_not_clamp() {
        let parent = root(30);
        let result = EffectiveDeadline::resolve(
            finite(30),
            None,
            CallTimeoutOverride::Omitted,
            Some(&parent),
            time(),
        )
        .unwrap();
        assert!(!result.ancestor_clamped);
        assert_eq!(result.source, RunLimitsPolicySource::GlobalDefault);
        let longer = RunLimitsRecord::admit_root(
            RunIdentity::new(),
            InvocationIdentity::new(),
            time(),
            RunLimitsConfig::default(),
            None,
            CallTimeoutOverride::from_optional(Some(0)),
        )
        .unwrap();
        let result = EffectiveDeadline::resolve(
            finite(30),
            None,
            CallTimeoutOverride::Omitted,
            Some(&longer),
            time(),
        )
        .unwrap();
        assert_eq!(result.deadline, parent.deadline);
        assert!(!result.ancestor_clamped);
    }

    #[test]
    fn checked_integer_duration_and_timestamp_overflow() {
        for secs in [u64::MAX, i64::MAX as u64] {
            assert_eq!(
                compute_deadline(time(), secs),
                Err(RunLimitsError::InvalidTimeoutSeconds(secs))
            );
            assert!(EffectiveDeadline::resolve(
                finite(secs),
                None,
                CallTimeoutOverride::Omitted,
                None,
                time()
            )
            .is_err());
        }
        assert!(matches!(
            compute_deadline(DateTime::<Utc>::MAX_UTC, 1),
            Err(RunLimitsError::DeadlineOverflow { .. })
        ));
    }

    #[test]
    fn serialization_preserves_identity_and_expiry_boundary() {
        let record = root(30);
        assert_ne!(record.run_id, RunIdentity::new());
        assert_ne!(record.invocation_id, InvocationIdentity::new());
        let loaded: RunLimitsRecord =
            serde_json::from_slice(&serde_json::to_vec(&record).unwrap()).unwrap();
        assert_eq!(record, loaded);
        assert_eq!(record.remaining(time()), Some(Duration::from_secs(30)));
        assert!(!record.is_expired_at(time()));
        assert!(record.is_expired_at(record.deadline.unwrap()));
        assert_eq!(record.remaining(record.deadline.unwrap()), None);
    }
}
