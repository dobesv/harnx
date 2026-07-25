//! Resolving a caller-supplied plan ID to a GitHub issue number.
//!
//! MCP callers address plans by the name they created them with, while this backend
//! numbers plans by issue number. The name lives on in `client_id` front-matter, so a
//! non-numeric plan ID is resolved by scanning the plan-labelled issues for it.

use harnx_mcp_plans_core::{PlanId, StoreError};

use crate::client::{GitHubClient, ListIssuesParams};
use crate::codec::DecodedPlan;

use super::{map_github_error, GitHubPlanStore};

impl GitHubPlanStore {
    /// Resolve a Plan ID to an issue number.
    ///
    /// Accepts either the canonical issue number or the client-provided plan name
    /// stored in `client_id` front-matter, which is what MCP callers address plans by.
    pub(super) async fn resolve_plan_number(
        &self,
        client: &GitHubClient,
        plan_id: &PlanId,
    ) -> Result<u64, StoreError> {
        match plan_id.parse::<u64>() {
            Ok(issue_number) => Ok(issue_number),
            Err(_) => self.find_plan_number_by_client_id(client, plan_id).await,
        }
    }

    /// Scan plan-labelled issues for one whose `client_id` front-matter matches `client_id`.
    ///
    /// Duplicates are resolved the same way reads are: most recently updated wins.
    async fn find_plan_number_by_client_id(
        &self,
        client: &GitHubClient,
        client_id: &str,
    ) -> Result<u64, StoreError> {
        let mut page = client
            .list_issues(ListIssuesParams {
                state: Some("open".to_string()),
                labels: Some(self.config.plan_label.clone()),
                per_page: Some(100),
                page: None,
            })
            .await
            .map_err(map_github_error)?;

        let mut best: Option<DecodedPlan> = None;
        loop {
            for issue in page.items {
                if !issue
                    .labels
                    .iter()
                    .any(|label| label.name == self.config.plan_label)
                {
                    continue;
                }
                let decoded = Self::decode_issue_to_plan(issue)?;
                if decoded.client_id.as_deref() != Some(client_id) {
                    continue;
                }
                let replace = best
                    .as_ref()
                    .is_none_or(|existing| Self::is_better_plan_candidate(existing, &decoded));
                if replace {
                    best = Some(decoded);
                }
            }
            let Some(next) = page.next.take() else {
                break;
            };
            page = client
                .list_issues_next(&next)
                .await
                .map_err(map_github_error)?;
        }

        best.ok_or(StoreError::NotFound)?
            .plan
            .id
            .parse::<u64>()
            .map_err(|_| StoreError::NotFound)
    }
}
