//! Real tool servers and HTTP inference shared by the result-boundary tests.
use super::*;
use anyhow::Context;
use harnx_core::instance::ServerScope;
use serde_json::Value;

#[path = "result_fixture/bridge.rs"]
mod bridge;
#[path = "result_fixture/provider.rs"]
mod provider;
#[path = "result_fixture/session.rs"]
mod session;

pub(super) use bridge::{BridgeScript, McpBridge};
pub(super) use provider::CapturedProvider;
pub(super) use session::{ProviderSession, SessionSpec};

pub(super) const BRIDGE_CAP: usize = 32768;

pub(super) struct ResultEnvironment {
    pub js: async_nats::jetstream::Context,
    pub scope: ServerScope,
    pub broker: common::NatsServerHandle,
    root: tempfile::TempDir,
    _env: Vec<EnvGuard>,
}

impl ResultEnvironment {
    pub async fn start() -> Result<Self> {
        let broker = spawn_nats_server().await?.context("nats-server required")?;
        let root = tempfile::tempdir()?;
        let env = vec![
            EnvGuard::set("HARNX_DATA_DIR", root.path().to_str().unwrap()),
            EnvGuard::set("HARNX_NATS_URL", broker.url()),
            EnvGuard::set("HARNX_NATS_TOKEN", ""),
        ];
        let js = async_nats::jetstream::new(async_nats::connect(broker.url()).await?);
        Ok(Self {
            js,
            scope: ServerScope::new(),
            broker,
            root,
            _env: env,
        })
    }
}
