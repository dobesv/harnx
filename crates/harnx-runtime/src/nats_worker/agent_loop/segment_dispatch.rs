use super::{
    run_agent_loop_segment, run_hitl_continuation_segment, AgentLoopSegmentArgs,
    HitlToolRoundContinuation, NatsAgentLoopOutcome,
};
use anyhow::Result;

impl AgentLoopSegmentArgs<'_> {
    pub(super) async fn run(
        self,
        continuation: Option<HitlToolRoundContinuation>,
    ) -> Result<NatsAgentLoopOutcome> {
        match continuation {
            Some(continuation) => Box::pin(run_hitl_continuation_segment(self, continuation)).await,
            None => Box::pin(run_agent_loop_segment(self)).await,
        }
    }
}
