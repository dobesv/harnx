use super::*;

pub(super) struct ToolsetFixture {
    pub(super) _nats: TestNats,
    pub(super) api: Arc<ReadyApi>,
    pub(super) caller: Arc<RecordingCaller>,
    pub(super) metadata: SessionMetadataStore,
    pub(super) bash: Arc<dyn Toolset>,
    pub(super) sandbox: Arc<dyn Toolset>,
}

impl ToolsetFixture {
    pub(super) async fn start(session_id: &str, binding: Option<&str>) -> Result<Option<Self>> {
        let Some(nats) = spawn_nats().await else {
            return Ok(None);
        };
        let client = async_nats::connect(&nats.url).await?;
        let jetstream = async_nats::jetstream::new(client);
        let metadata = SessionMetadataStore::ensure(&jetstream, 1).await?;
        metadata
            .create(&SessionMetadata::new(
                session_id,
                SessionInitializer::named("coder", Default::default()),
            ))
            .await?;
        if let Some(sandbox_id) = binding {
            metadata
                .replace_tool_context_value(
                    ToolContextEntry {
                        session_id,
                        key: SANDBOX_CONTEXT_KEY,
                    },
                    json!({"version": 1, "sandbox_id": sandbox_id}),
                )
                .await?;
        }
        let api = Arc::new(ReadyApi::default());
        let caller = Arc::new(RecordingCaller::default());
        let toolsets = sandbox_toolsets(
            SandboxManager::new(api.clone(), Default::default()),
            caller.clone(),
            metadata.clone(),
        );
        let find = |name| {
            toolsets
                .iter()
                .find(|toolset| toolset.name() == name)
                .cloned()
                .unwrap()
        };
        Ok(Some(Self {
            _nats: nats,
            api,
            caller,
            metadata,
            bash: find("bash"),
            sandbox: find("sandbox"),
        }))
    }
}

pub(super) async fn bound_sandbox(
    metadata: &SessionMetadataStore,
    session_id: &str,
) -> Result<SandboxBinding> {
    Ok(serde_json::from_value(
        metadata.get_tool_context(session_id).await?.unwrap().values[SANDBOX_CONTEXT_KEY].clone(),
    )?)
}
