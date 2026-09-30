use super::{SandboxPorts, *};

pub(super) struct ToolsetFixture {
    pub(super) _nats: TestNats,
    pub(super) api: Arc<ReadyApi>,
    pub(super) caller: Arc<RecordingCaller>,
    pub(super) metadata: SessionMetadataStore,
    pub(super) bash: Arc<dyn Toolset>,
    pub(super) fs: Arc<dyn Toolset>,
    pub(super) sandbox: Arc<dyn Toolset>,
}

impl ToolsetFixture {
    pub(super) async fn start(session_id: &str, binding: Option<&str>) -> Result<Option<Self>> {
        Self::start_with_names(session_id, binding, SandboxToolsetNames::default()).await
    }

    pub(super) async fn start_with_names(
        session_id: &str,
        binding: Option<&str>,
        names: SandboxToolsetNames,
    ) -> Result<Option<Self>> {
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
                        session_id: &harnx_core::session_identity::session_key(
                            Some("coder"),
                            session_id,
                        ),
                        key: SANDBOX_CONTEXT_KEY,
                    },
                    json!({"version": 1, "sandbox_id": sandbox_id}),
                )
                .await?;
        }
        let api = Arc::new(ReadyApi::default());
        let caller = Arc::new(RecordingCaller::default());
        let mut toolsets = sandbox_toolsets_with_names(
            SandboxManager::new(api.clone(), Default::default()),
            caller.clone(),
            metadata.clone(),
            SandboxPorts::default(),
            names,
        );
        let sandbox = toolsets.pop().unwrap();
        let fs = toolsets.pop().unwrap();
        let bash = toolsets.pop().unwrap();
        Ok(Some(Self {
            _nats: nats,
            api,
            caller,
            metadata,
            bash,
            fs,
            sandbox,
        }))
    }
}

pub(super) async fn bound_sandbox(
    metadata: &SessionMetadataStore,
    session_id: &str,
) -> Result<SandboxBinding> {
    Ok(serde_json::from_value(
        metadata
            .get_tool_context(&harnx_core::session_identity::session_key(
                Some("coder"),
                session_id,
            ))
            .await?
            .unwrap()
            .values[SANDBOX_CONTEXT_KEY]
            .clone(),
    )?)
}
