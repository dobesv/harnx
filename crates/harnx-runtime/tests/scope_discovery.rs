#[allow(dead_code)]
mod common;

use common::spawn_nats_server;
use harnx_core::instance::{ServerScope, SHARED_SCOPE};

/// A mismatched scope is silent otherwise: the worker scans one prefix, a
/// typo'd server registers under another, and the worker just sees no tools.
#[tokio::test]
async fn discovery_reports_the_scope_when_nothing_is_registered() {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await.expect("spawn nats") else {
        eprintln!("skipping: no nats-server binary available");
        return;
    };
    let client = async_nats::connect(server.url())
        .await
        .expect("connect to nats");
    let scope = ServerScope::from_string(SHARED_SCOPE);

    let report = harnx_runtime::nats_tool_provider::describe_discovery(&client, &scope)
        .await
        .expect("describe discovery");

    assert_eq!(report.found, 0);
    assert!(
        report.message.contains(SHARED_SCOPE),
        "a zero-result discovery must name the scope it searched; got: {}",
        report.message
    );
}
