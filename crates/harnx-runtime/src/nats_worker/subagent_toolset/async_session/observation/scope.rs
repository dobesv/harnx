use super::{
    budget::{Budget, Stop},
    dispatch::trusted_parent_scope,
    PromptHandle, SubagentToolset,
};
use crate::nats_session_metadata::{AdmissionOrigin, InvocationAdmission};
use harnx_toolset::{ToolInvocationContext, ToolInvokeError};

pub(super) struct Scope {
    pub(super) storage: String,
    pub(super) admission: InvocationAdmission,
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
        (id.as_deref(), Some(content))
            == (
                Some(self.prompt_id.as_str()),
                self.admission.prompt_content.as_ref(),
            )
    }

    pub(super) async fn resolve(
        toolset: &SubagentToolset,
        handle: &PromptHandle,
        context: &ToolInvocationContext,
        budget: &Budget,
    ) -> Result<Self, ScopeError> {
        let parent = trusted_parent_scope(context).map_err(ScopeError::Denied)?;
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
        let inherited =
            admission.origin == AdmissionOrigin::Inherited && admission.parent.is_some();
        let parent_matches = admission.parent_storage_key.as_deref() == Some(&parent);
        if admission.fixed_ticket.is_some() {
            return Err(denied());
        }
        if !inherited || !parent_matches {
            return Err(denied());
        }
        // Execution identity is diagnostic only. The raw scan still has to
        // find this exact user_msg_id and verify its original admitted content.
        Ok(Self {
            storage,
            admission,
            prompt_id: handle.prompt_id.clone(),
            call_id: context.call_id.clone(),
        })
    }
}

fn denied() -> ScopeError {
    ScopeError::Denied(ToolInvokeError::Recoverable(
        "prompt handle is not an authorized async dispatch in this parent scope".into(),
    ))
}
