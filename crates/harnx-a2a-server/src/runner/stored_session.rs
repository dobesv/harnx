//! Internal session opening preserves stored ownership without inventing ACL facts.
use super::*;
use crate::store::{validate_stored_owner, TaskAllocation};

/// Only allocation and background recovery construct this request. Memberships
/// are restored from the allocation's stored snapshot, never from live state:
/// recovery can settle retained work, never admit a new prompt.
pub(super) struct StoredSessionRequest<'a> {
    pub export: &'a Export,
    pub owner: &'a Principal,
    /// The caller-principals snapshot recorded with the allocation's
    /// reservation; `None` for active recovery of an existing session and for
    /// records written before snapshots existed.
    pub principals: Option<&'a harnx_core::user_principals::UserPrincipals>,
    pub global_config: &'a GlobalConfig,
    pub activation_route: SessionActivationRoute,
    pub abort: AbortSignal,
    pub allow_create: bool,
}

impl Runner {
    /// Trusted reservation path; public context IDs still go through request ACLs.
    pub(crate) async fn allocated_session(
        &self,
        request: SessionRequest<'_>,
        allocation: &TaskAllocation,
        principals: Option<&harnx_core::user_principals::UserPrincipals>,
    ) -> Result<NatsSession> {
        let binding = self.store.get_binding(&allocation.storage_key).await?;
        if let Some(binding) = &binding {
            anyhow::ensure!(
                crate::store::validate_binding(
                    binding,
                    request.export,
                    request.owner,
                    self.store.access_rules()
                ),
                StoreError::NotFound
            );
        } else if let Some(rules) = self.store.access_rules() {
            let caller = request.owner.caller();
            anyhow::ensure!(
                request.owner.principal.user_id().is_some()
                    && rules.can_create_session(&request.export.agent_ref(), caller.view()),
                StoreError::NotFound
            );
        }
        self.stored_session(
            StoredSessionRequest {
                export: request.export,
                owner: &request.owner.principal,
                principals,
                global_config: request.global_config,
                activation_route: request.activation_route,
                abort: request.abort,
                allow_create: true,
            },
            allocation,
        )
        .await
    }

    pub(super) async fn stored_session(
        &self,
        request: StoredSessionRequest<'_>,
        allocation: &TaskAllocation,
    ) -> Result<NatsSession> {
        anyhow::ensure!(
            allocation.storage_key
                == harnx_core::session_identity::session_key(
                    Some(&request.export.agent),
                    &allocation.local_id
                ),
            "reservation session identity mismatch"
        );
        assert_local_id_no_dot(&allocation.local_id)?;
        let binding = self.store.get_binding(&allocation.storage_key).await?;
        if let Some(binding) = &binding {
            anyhow::ensure!(
                validate_stored_owner(binding, request.export, request.owner),
                StoreError::NotFound
            );
        } else {
            anyhow::ensure!(request.allow_create, StoreError::NotFound);
        }
        let initializer = session_initializer(
            request.export,
            request.owner,
            request.principals,
            binding.as_ref().map(|_| allocation.local_id.as_str()),
        )?;
        let session = Box::pin(NatsSession::from_global_config(
            NatsSessionConfig {
                cluster: request
                    .export
                    .cluster
                    .clone()
                    .unwrap_or_else(|| "__local__".into()),
                initializer,
                session_id: Some(allocation.local_id.clone()),
                activation_route: request.activation_route,
            },
            request.global_config,
            request.abort,
        ))
        .await?;
        if binding.is_none() {
            if let Err(error) = self
                .store
                .bind_context(session.storage_key(), request.export, request.owner)
                .await
            {
                let bound = self.store.get_binding(session.storage_key()).await?;
                anyhow::ensure!(
                    bound.is_some_and(|binding| {
                        validate_stored_owner(&binding, request.export, request.owner)
                    }),
                    error
                );
            }
        }
        Ok(session)
    }
}
