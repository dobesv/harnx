use crate::config::Config;
use crate::nats_session_metadata::{
    CallTimeoutOverride, EffectiveDeadline, RunLimitsError, RunLimitsRecord,
};
use chrono::{DateTime, Utc};
use harnx_core::config_data::ResolvedRunLimits;

impl Config {
    /// Resolve effective run limits from global and agent configuration.
    ///
    /// This merges the global `run_limits` configuration with the per-agent
    /// override from the agent's front matter.
    ///
    /// Returns a `ResolvedRunLimits` containing the effective timeout policy.
    pub fn resolve_run_limits(
        &self,
        agent_config: Option<&harnx_core::agent_config::AgentConfig>,
    ) -> ResolvedRunLimits {
        let global = self.data.run_limits;

        // Get the agent's run_limits override, if any
        let agent_override = agent_config
            .and_then(|a| a.run_limits())
            .and_then(|o| o.timeout_secs);

        global.resolve(agent_override.as_ref())
    }
    /// Resolve a new admission against the target's effective config. Replay must
    /// load the persisted record instead of calling this with a new timestamp.
    pub fn resolve_run_deadline(
        &self,
        target: Option<&harnx_core::agent_config::AgentConfig>,
        call: CallTimeoutOverride,
        parent: Option<&RunLimitsRecord>,
        admitted_at: DateTime<Utc>,
    ) -> Result<EffectiveDeadline, RunLimitsError> {
        EffectiveDeadline::resolve(self.data.run_limits, target, call, parent, admitted_at)
    }
}
