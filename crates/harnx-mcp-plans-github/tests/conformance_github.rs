//! Conformance test suite for GitHubPlanStore against shared conformance suite.
//!
//! This test drives the stateful GitHub API mock through the universal
//! `run_conformance` harness with GitHub capability flags.

mod github_mock;

use std::sync::Arc;

use harnx_mcp_plans_core::conformance::{run_conformance, BackendCapabilities};

use github_mock::create_mock_store_and_server;

#[tokio::test]
async fn github_mock_server_works_for_supported_operations() {
    let store = Arc::new(create_mock_store_and_server().await);

    run_conformance(
        store,
        BackendCapabilities {
            preserves_client_id: false,
            deletes_permanently: false,
            rejects_invalid_create_ids: false,
        },
    )
    .await;
}
