//! Recovery identity is immutable owner/export/storage, never request memberships.
use super::*;

impl RecoveryRegistration {
    pub(crate) fn validate_export(&self, export: &Export) -> Result<()> {
        ensure!(
            (&self.agent, &self.export) == (&export.agent, &export.public_name),
            "recovery export mismatch"
        );
        if let Some(first) = &self.first {
            let actual = (
                &first.identity.owner,
                first.identity.export.as_str(),
                first.identity.cluster.as_str(),
                &first.allocation,
            );
            let expected = (
                &self.owner,
                export.public_name.as_str(),
                export.cluster.as_deref().unwrap_or("__local__"),
                &self.allocation,
            );
            ensure!(actual == expected, "recovery reservation identity mismatch");
        }
        Ok(())
    }

    pub(super) fn identity_matches(&self, other: &Self) -> bool {
        let actual = (
            &self.agent,
            &self.export,
            &self.owner,
            &self.allocation.storage_key,
        );
        let expected = (
            &other.agent,
            &other.export,
            &other.owner,
            &other.allocation.storage_key,
        );
        actual == expected
    }
}

impl A2aStore {
    pub(super) async fn recovery_owner(
        &self,
        export: &Export,
        caller: &Principal,
        allocation: &TaskAllocation,
    ) -> Result<Option<String>> {
        let Some(binding) = self.get_binding(&allocation.storage_key).await? else {
            return Ok(caller.user_id().map(str::to_owned));
        };
        let owner = binding
            .owner
            .clone()
            .map(Principal::User)
            .unwrap_or(Principal::Anonymous);
        ensure!(
            super::super::validate_stored_owner(&binding, export, &owner),
            "recovery binding mismatch"
        );
        Ok(binding.owner)
    }
}

#[cfg(test)]
mod tests;
