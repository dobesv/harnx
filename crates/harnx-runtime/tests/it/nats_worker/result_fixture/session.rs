use super::*;
use harnx_core::model::{Model, ModelData};
use harnx_runtime::{agent_loop::OnToolRoundFn, config::GlobalConfig};

pub(in super::super) struct SessionSpec<'a> {
    pub id: &'a str,
    pub api_base: &'a str,
    pub supports_vision: bool,
}

pub(in super::super) struct ProviderSession {
    pub config: GlobalConfig,
    pub key: String,
    metadata: SessionMetadataStore,
    scope: ServerScope,
}

impl ProviderSession {
    pub async fn create(env: &ResultEnvironment, spec: SessionSpec<'_>) -> Result<Self> {
        let mut config = provider_config(env.broker.url(), &spec)?;
        let store = SessionMetadataStore::ensure(&env.js, 1).await?;
        let metadata = SessionMetadata::new(
            spec.id,
            SessionInitializer::inline(
                "",
                Default::default(),
                SessionOverrides {
                    model: Some("mock:test".into()),
                    use_tools: Some(vec!["*".into()]),
                    ..Default::default()
                },
            ),
        );
        store.create(&metadata).await?;
        let mut session = metadata.base_session();
        session.set_model(config.model.clone());
        config.session = Some(session);
        Ok(Self {
            config: Arc::new(ConfigLock::new(config)),
            key: metadata.storage_key(),
            metadata: store,
            scope: env.scope.clone(),
        })
    }

    pub async fn run_turn(&self, on_tool_round: Option<OnToolRoundFn>) -> Result<()> {
        let input = harnx_runtime::config::input::from_str(&self.config, "inspect results", None);
        tokio::time::timeout(
            CI_SAFE_TIMEOUT,
            run_agent_loop_with_nats(RunAgentLoopArgs {
                cluster_key: "local",
                manage_servers: false,
                session_id: &self.key,
                config: self.config.clone(),
                instance_id: self.scope.clone(),
                initial_input: input,
                abort_signal: create_abort_signal(),
                token_budget: None,
                call_fn: None,
                lease: None,
                activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
                event_sink: None,
                after_seq_observer: None,
                session_metadata: Some(&self.metadata),
                on_tool_round,
                working_dir: None,
            }),
        )
        .await??;
        Ok(())
    }

    pub async fn replay(&self) -> Result<()> {
        // Force reconstruction from the durable transcript, not in-memory messages.
        self.config
            .write()
            .session
            .as_mut()
            .unwrap()
            .messages
            .clear();
        self.run_turn(None).await
    }
}

fn provider_config(url: &str, spec: &SessionSpec<'_>) -> Result<Config> {
    let model: ModelData = serde_json::from_value(json!({
        "name": "test", "max_input_tokens": 32000, "max_output_tokens": 1024,
        "supports_vision": spec.supports_vision, "supports_tool_use": true
    }))?;
    let mut config = local_nats_config(NatsServerSpec {
        name: "local",
        url,
        token: None,
    });
    config.stream = false;
    config.tool_use = true;
    config.use_tools = Some(vec!["*".into()]);
    // Bounds must come from result handling, not summarization before inference.
    config.compress_threshold = 0;
    config.model = Model::from_config("mock", std::slice::from_ref(&model)).remove(0);
    config.clients = vec![serde_json::from_value(json!({
        "type": "openai-compatible", "api_base": spec.api_base, "api_key": "test-key",
        "models": [model]
    }))?];
    config.clients[0].set_name("mock".into());
    Ok(config)
}
