//! Local ownership follows successful registration revisions, not task completion order.

use super::Lifecycle;
use harnx_credentials::Principal;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ManagedRegistration {
    pub connection_id: String,
    pub revision: u64,
}

impl Lifecycle {
    pub(super) async fn track_registration(
        &self,
        principal: &Principal,
        connection_id: &str,
        revision: u64,
    ) {
        let mut managed = self.managed.lock().await;
        if managed
            .get(principal)
            .is_none_or(|current| current.revision < revision)
        {
            managed.insert(
                principal.clone(),
                ManagedRegistration {
                    connection_id: connection_id.into(),
                    revision,
                },
            );
        }
    }

    pub(super) async fn forget_connection(&self, principal: &Principal, connection_id: &str) {
        let mut managed = self.managed.lock().await;
        if managed
            .get(principal)
            .is_some_and(|current| current.connection_id == connection_id)
        {
            managed.remove(principal);
        }
    }

    pub(super) async fn forget_registration(
        &self,
        principal: &Principal,
        expected: &ManagedRegistration,
    ) {
        let mut managed = self.managed.lock().await;
        if managed.get(principal) == Some(expected) {
            managed.remove(principal);
        }
    }
}
