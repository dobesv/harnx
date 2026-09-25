//! Shared ACP/NATS test harness components.

mod broker;
mod config;
mod messages;
mod session;
mod transport;
mod worker;

pub(crate) use broker::spawn_nats_server;
pub(crate) use config::{new_agent, test_config};
pub(crate) use messages::{notification_text, spawn_prompt, text_prompt};
pub(crate) use session::initialize_and_create_session;
pub(crate) use transport::{attach_test_client, PermissionReply, TestClient};
pub(crate) use worker::spawn_worker;

pub(crate) const TOKEN: &str = "acp-test-token";
pub(crate) const CLUSTER: &str = "acp-test";
pub(crate) const AGENT_NAME: &str = "acp-test-agent";
pub(crate) const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
