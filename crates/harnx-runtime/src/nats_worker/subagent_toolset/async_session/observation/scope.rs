use super::{
    budget::{Budget, Stop},
    dispatch::trusted_parent_scope,
    PromptHandle, SubagentToolset,
};
use crate::nats_session_metadata::{InvocationAdmission, JoinedDispatch};
use harnx_toolset::{ToolInvocationContext, ToolInvokeError};

pub(super) struct Scope {
    pub(super) storage: String,
    pub(super) admission: InvocationAdmission,
    /// Joined-dispatch record: authoritative for the original prompt content
    /// when this prompt joined a busy turn
    /// (issue #2672). The bound invocation's admission still supplies the
    /// execution facts (invocation id, run identity, terminal classification).
    pub(super) joined: Option<JoinedDispatch>,
    pub(super) prompt_id: String,
    pub(super) call_id: String,
}

pub(super) enum ScopeError {
    Stop(Stop),
    Denied(ToolInvokeError),
}

impl From<Stop> for ScopeError {
    fn from(value: Stop) -> Self {
        Self::Stop(value)
    }
}

impl Scope {
    /// The original admitted content this prompt must match: the joined
    /// record's content when one exists, otherwise the invocation's.
    pub(super) fn prompt_content(&self) -> Option<&harnx_core::message::MessageContent> {
        self.joined
            .as_ref()
            .map(|joined| &joined.prompt_content)
            .or(self.admission.prompt_content.as_ref())
    }

    pub(super) fn matches_prompt(&self, entry: &harnx_core::session::SessionLogEntry) -> bool {
        let harnx_core::session::SessionLogEntry::Message {
            id, role, content, ..
        } = entry
        else {
            return false;
        };
        if !role.is_user() {
            return false;
        }
        (id.as_deref(), Some(content)) == (Some(self.prompt_id.as_str()), self.prompt_content())
    }

    pub(super) async fn resolve(
        toolset: &SubagentToolset,
        handle: &PromptHandle,
        context: &ToolInvocationContext,
        budget: &Budget,
    ) -> Result<Self, ScopeError> {
        // Require attested transport context, not ownership of the dispatch.
        trusted_parent_scope(context).map_err(ScopeError::Denied)?;
        // Local IDs are not storage keys. Tool prefix alone supplies the agent
        // namespace, including when the caller spells another agent in the ID.
        let storage =
            harnx_core::session_identity::session_key(Some(&toolset.agent), &handle.session_id);
        let admission = budget
            .io(toolset
                .session_metadata
                .prompt_admission(&storage, &handle.prompt_id))
            .await?
            .map_err(|_| ScopeError::Stop(Stop::Unavailable))?
            .ok_or_else(denied)?;
        // Observation is namespace-wide. The joined record supplies content,
        // but must still agree with the prompt's durable invocation binding.
        let joined = budget
            .io(toolset
                .session_metadata
                .joined_dispatch(&storage, &handle.prompt_id))
            .await?
            .map_err(|_| ScopeError::Stop(Stop::Unavailable))?;
        if admission.fixed_ticket.is_some() {
            return Err(denied());
        }
        if joined
            .as_ref()
            .is_some_and(|joined| joined.invocation_id != admission.invocation_id.as_str())
        {
            return Err(denied());
        }
        // Execution identity is diagnostic only. The raw scan still has to
        // find this exact user_msg_id and verify its original admitted content.
        Ok(Self {
            storage,
            admission,
            joined,
            prompt_id: handle.prompt_id.clone(),
            call_id: context.call_id.clone(),
        })
    }
}

fn denied() -> ScopeError {
    ScopeError::Denied(ToolInvokeError::Recoverable(
        "prompt handle has no valid async dispatch binding in this agent namespace".into(),
    ))
}
