//! Bridge and runtime configuration fixtures.

use std::sync::Arc;

use harnx_core::agent_config::AgentConfig;
use harnx_runtime::config::{Config, GlobalConfig, NatsServerConfig};
use harnx_runtime::{SessionActivationRoute, SessionInitializer};

use super::{AGENT_NAME, CLUSTER, TOKEN};

pub(crate) fn test_config(url: &str) -> GlobalConfig {
    let mut agent = AgentConfig::from_markdown(
        AGENT_NAME,
        "---\nmodel: test:test-model\n---\nACP integration test agent",
    )
    .expect("parse test agent");
    agent.set_resolved_model(harnx_core::model::Model::new("test", "test-model"));

    Arc::new(parking_lot::RwLock::new(Config {
        data: harnx_core::config_data::ConfigData {
            model_id: "test:test-model".to_string(),
            dry_run: false,
            ..Default::default()
        },
        agent: Some(harnx_runtime::config::Agent::new(agent)),
        model: harnx_core::model::Model::new("test", "test-model"),
        nats_servers: vec![NatsServerConfig {
            name: CLUSTER.to_string(),
            url: url.to_string(),
            token: Some(TOKEN.to_string()),
            replicas: None,
            tls: Some(false),
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            ignore_discovered_servers: None,
            agents: vec![],
        }],
        ..Default::default()
    }))
}
pub(crate) fn new_agent(config: &GlobalConfig) -> Arc<harnx_acp_server::HarnxAgent> {
    let nats_config = harnx_acp_server::NatsAgentConfig {
        runtime_config: Arc::clone(config),
        cluster: CLUSTER.to_string(),
        activation_route: SessionActivationRoute::ClusterShared,
        session_initializer: SessionInitializer::inline(
            "ACP integration test agent",
            Default::default(),
            Default::default(),
        ),
    };
    Arc::new(harnx_acp_server::HarnxAgent::with_nats_config(
        AGENT_NAME.to_string(),
        nats_config,
    ))
}
