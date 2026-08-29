#[allow(dead_code)]
mod common;

use anyhow::Result;
use async_trait::async_trait;
use harnx_core::abort::create_abort_signal;
use harnx_core::execution_context::{
    ExecutionContextObservation, GitRemoteObservation, GitRepositoryObservation,
    EXECUTION_CONTEXT_NAMESPACE,
};
use harnx_core::instance::{ServerScope, HARNX_SERVER_SCOPE};
use harnx_core::tool::{ToolCall, ToolError, ToolProvider, ToolResult};
use harnx_runtime::config::{Config, HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV};
use harnx_runtime::nats_session_metadata::{
    execution_contexts, SessionInitializer, SessionMetadata, SessionMetadataStore,
};
use harnx_runtime::nats_tool_provider::{NatsInFlightCalls, NatsToolProvider};
use harnx_runtime::nats_worker::NatsSessionLogBackend;
use harnx_time_server::TimeToolset;
use harnx_toolset::{server_identity_token, Registration, ToolInvokeError, ToolSpec, Toolset};
use harnx_toolset_server::{
    registration_key, serve_over_nats, TOOL_PROTOCOL_VERSION, TOOL_REGISTRY_BUCKET,
    TOOL_SCHEMA_VERSION,
};
use parking_lot::RwLock;
use serde_json::json;
use std::ffi::OsString;
use std::sync::Arc;
use std::time::{Duration, Instant};

const TOKEN: &str = "nats-tool-provider-test-token";

struct ExecutionContextToolset {
    observation: ExecutionContextObservation,
}

#[async_trait]
impl Toolset for ExecutionContextToolset {
    fn name(&self) -> &str {
        "context"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "observe_execution_context".to_string(),
            description: "Return a test execution context".to_string(),
            input_schema: json!({"type": "object"}),
            idempotent_hint: true,
            read_only_hint: true,
            timeout_secs: None,
            meta: None,
        }]
    }

    async fn invoke(
        &self,
        tool: &str,
        _args: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> std::result::Result<serde_json::Value, ToolInvokeError> {
        assert_eq!(tool, "observe_execution_context");
        Ok(json!({
            "content": [{"type": "text", "text": "observed"}],
            "_meta": {
                EXECUTION_CONTEXT_NAMESPACE: self.observation
            }
        }))
    }
}

struct EnvGuard {
    url: Option<OsString>,
    token: Option<OsString>,
    instance_id: Option<OsString>,
}

impl EnvGuard {
    fn install(url: &str, token: &str, instance_id: &ServerScope) -> Self {
        let guard = Self {
            url: std::env::var_os(HARNX_NATS_URL_ENV),
            token: std::env::var_os(HARNX_NATS_TOKEN_ENV),
            instance_id: std::env::var_os(HARNX_SERVER_SCOPE),
        };
        unsafe {
            std::env::set_var(HARNX_NATS_URL_ENV, url);
            std::env::set_var(HARNX_NATS_TOKEN_ENV, token);
            std::env::set_var(HARNX_SERVER_SCOPE, instance_id.as_str());
        }
        guard
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            match self.url.take() {
                Some(value) => std::env::set_var(HARNX_NATS_URL_ENV, value),
                None => std::env::remove_var(HARNX_NATS_URL_ENV),
            }
            match self.token.take() {
                Some(value) => std::env::set_var(HARNX_NATS_TOKEN_ENV, value),
                None => std::env::remove_var(HARNX_NATS_TOKEN_ENV),
            }
            match self.instance_id.take() {
                Some(value) => std::env::set_var(HARNX_SERVER_SCOPE, value),
                None => std::env::remove_var(HARNX_SERVER_SCOPE),
            }
        }
    }
}

async fn wait_for_registry(
    client: &async_nats::Client,
    instance_id: &ServerScope,
    identity: &str,
) -> Result<()> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(store) = jetstream.get_key_value(TOOL_REGISTRY_BUCKET).await {
            if store
                .get(registration_key(instance_id, identity))
                .await?
                .is_some()
            {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for tool registration '{identity}'");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn spawn_execution_context_server(
    instance_id: ServerScope,
    server_url: String,
    workspace: &std::path::Path,
) -> tokio::task::JoinHandle<Result<()>> {
    let mut observation = ExecutionContextObservation::observe(workspace, workspace);
    observation.repository = Some(GitRepositoryObservation {
        worktree_root: workspace.to_string_lossy().into_owned(),
        branch: Some("feature".to_string()),
        remotes: vec![GitRemoteObservation {
            name: "origin".to_string(),
            repository: "github.com/acme/context-repo".to_string(),
            primary: true,
        }],
    });
    tokio::spawn(async move {
        serve_over_nats(
            ExecutionContextToolset { observation },
            instance_id,
            &server_url,
            TOKEN,
        )
        .await
    })
}

async fn set_wait_timeout(
    client: &async_nats::Client,
    instance_id: &ServerScope,
    timeout_secs: u64,
) -> Result<()> {
    let store = async_nats::jetstream::new(client.clone())
        .get_key_value(TOOL_REGISTRY_BUCKET)
        .await?;
    let key = registration_key(instance_id, "____time");
    let value = store
        .get(&key)
        .await?
        .expect("time tool registration exists");
    let mut registration: Registration = serde_json::from_slice(&value)?;
    registration
        .tools
        .iter_mut()
        .find(|tool| tool.name == "wait")
        .expect("wait tool is registered")
        .timeout_secs = Some(timeout_secs);
    store
        .put(&key, serde_json::to_vec(&registration)?.into())
        .await?;
    Ok(())
}

async fn add_collision_registration(
    client: &async_nats::Client,
    instance_id: &ServerScope,
) -> Result<()> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let store = jetstream.get_key_value(TOOL_REGISTRY_BUCKET).await?;
    let registration = Registration {
        package: None,
        config: String::new(),
        server: "collision".to_string(),
        tools: vec![ToolSpec {
            name: harnx_runtime::session_history::TOOL_NAME.to_string(),
            description: "NATS collision test".to_string(),
            input_schema: json!({ "type": "object" }),
            idempotent_hint: true,
            read_only_hint: true,
            timeout_secs: None,
            meta: None,
        }],
        schema_version: TOOL_SCHEMA_VERSION,
        proto_version: TOOL_PROTOCOL_VERSION,
    };
    store
        .put(
            registration_key(
                instance_id,
                &server_identity_token(None, "", &registration.server),
            ),
            serde_json::to_vec(&registration)?.into(),
        )
        .await?;
    Ok(())
}

async fn add_duplicate_registrations(
    client: &async_nats::Client,
    instance_id: &ServerScope,
) -> Result<()> {
    let store = async_nats::jetstream::new(client.clone())
        .get_key_value(TOOL_REGISTRY_BUCKET)
        .await?;
    for server in ["alpha", "beta"] {
        let registration = Registration {
            package: None,
            config: String::new(),
            server: server.to_string(),
            tools: vec![ToolSpec {
                name: "duplicate_tool".to_string(),
                description: format!("owned by {server}"),
                input_schema: json!({ "type": "object" }),
                idempotent_hint: true,
                read_only_hint: true,
                timeout_secs: Some(30),
                meta: None,
            }],
            schema_version: TOOL_SCHEMA_VERSION,
            proto_version: TOOL_PROTOCOL_VERSION,
        };
        store
            .put(
                registration_key(instance_id, &server_identity_token(None, "", server)),
                serde_json::to_vec(&registration)?.into(),
            )
            .await?;
    }
    Ok(())
}
fn assert_declarations_and_collisions(provider: &NatsToolProvider) {
    assert_eq!(provider.name(), "nats");
    for tool in ["get_current_time", "convert_time", "wait", "wait_until"] {
        assert!(
            provider.has_tool(tool),
            "missing registered pilot tool {tool}"
        );
    }
    assert_eq!(
        provider
            .declarations()
            .iter()
            .filter(|tool| tool.name.ends_with("_duplicate_tool"))
            .count(),
        2
    );
    assert!(provider
        .declarations_for_use_tools(Some("beta"))
        .iter()
        .any(|tool| tool.name == "beta_duplicate_tool"));
    assert!(provider
        .declarations_for_use_tools(Some("alpha"))
        .iter()
        .any(|tool| tool.name == "alpha_duplicate_tool"));
}

async fn assert_invocation_and_per_call_timeout(provider: &NatsToolProvider) -> Result<()> {
    let result = provider
        .call_tool(
            "get_current_time",
            json!({ "timezone": "UTC" }),
            &create_abort_signal(),
        )
        .await
        .map_err(tool_error)?;
    assert_eq!(result["timezone"], "UTC");

    let started = Instant::now();
    let result = provider
        .call_tool("wait", json!({ "seconds": 1.2 }), &create_abort_signal())
        .await
        .map_err(tool_error)?;
    assert_eq!(result["message"], "Waited 1.2 seconds");
    assert!(started.elapsed() >= Duration::from_millis(1200));
    Ok(())
}

async fn assert_context_flows_from_tool_to_session_enumeration(
    provider: &NatsToolProvider,
    client: &async_nats::Client,
) -> Result<()> {
    let call = ToolCall::new(
        "observe_execution_context".to_string(),
        json!({}),
        Some("context-call".to_string()),
        None,
    );
    let provider_output = provider
        .call_tool(&call.name, call.arguments.clone(), &create_abort_signal())
        .await
        .map_err(tool_error)?;
    assert!(provider_output
        .value
        .get("_meta")
        .and_then(|meta| meta.get(EXECUTION_CONTEXT_NAMESPACE))
        .is_none());
    let mut tool_result = ToolResult::new(call.clone(), provider_output.value);
    tool_result.execution_context = provider_output.execution_context;

    let jetstream = async_nats::jetstream::new(client.clone());
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let session_id = format!("tool-context-{}", uuid::Uuid::new_v4());
    let metadata = SessionMetadata::new(
        &session_id,
        SessionInitializer::named("metis", Default::default()),
    );
    store.create(&metadata).await?;
    let backend =
        NatsSessionLogBackend::new(jetstream, &session_id).with_metadata_store(Some(store.clone()));
    let mut session = metadata.base_session();
    let sink = Arc::new(backend) as Arc<dyn harnx_runtime::config::session::SessionAppendSink>;
    session.runtime = Some(Arc::new(sink));
    let config = Arc::new(RwLock::new(Config::default()));
    let input = harnx_runtime::config::input::from_str(
        &config,
        "inspect context",
        Some(config.read().extract_agent()),
    );
    harnx_runtime::config::session::add_tool_calls(
        &mut session,
        &input,
        "observing",
        None,
        std::slice::from_ref(&call),
    )?;
    harnx_runtime::config::session::add_tool_results(&mut session, &[tool_result])?;

    let listed = store.list().await?;
    let retained = listed
        .iter()
        .find(|listed| listed.metadata.session_id == session_id)
        .map(|listed| execution_contexts(&listed.metadata))
        .transpose()?
        .expect("session appears in canonical enumeration");
    assert_eq!(retained.len(), 1);
    assert_eq!(
        retained[0].primary_repository(),
        Some("github.com/acme/context-repo")
    );
    assert_eq!(retained[0].branch(), Some("feature"));
    Ok(())
}

async fn assert_per_call_timeout_enforced(provider: &NatsToolProvider) -> Result<()> {
    let started = Instant::now();
    let result = provider
        .call_tool("wait", json!({ "seconds": 2.0 }), &create_abort_signal())
        .await;
    let elapsed = started.elapsed();

    match result {
        Err(ToolError::Recoverable(error)) => {
            assert!(error.to_string().contains("timed out"), "{error:#}");
        }
        Err(ToolError::Fatal(error)) => {
            anyhow::bail!("per-call timeout returned a fatal error: {error:#}")
        }
        Ok(value) => anyhow::bail!("per-call timeout unexpectedly succeeded: {value}"),
    }
    assert!(elapsed >= Duration::from_secs(1), "timed out too early");
    assert!(
        elapsed < Duration::from_secs(2),
        "timed out too late: {elapsed:?}"
    );
    Ok(())
}

async fn assert_context_declarations_and_precedence(instance_id: &ServerScope) {
    let config = Arc::new(RwLock::new(Config::default()));
    let context = harnx_runtime::tool::build_tool_eval_context(
        harnx_runtime::tool::BuildToolEvalContextParams::new(&config, instance_id)
            .with_agent_use_tools(Some("*")),
    )
    .await;
    let declarations = &context.render.as_ref().expect("render context").decl_map;
    for raw_tool in ["get_current_time", "convert_time", "wait", "wait_until"] {
        let visible_tool = format!("time_{raw_tool}");
        assert!(
            declarations.contains_key(&visible_tool),
            "pilot tool not declared: {visible_tool}"
        );
        assert!(context.allowed_tool_names.contains(&visible_tool));
    }
    let collision = harnx_runtime::session_history::TOOL_NAME;
    assert!(context.providers[0].has_tool(collision));
    assert_eq!(context.providers[0].name(), "nats");
    assert!(context
        .providers
        .iter()
        .skip(1)
        .any(|provider| provider.has_tool(collision)));
}

async fn assert_abort_and_supervisor_failure(provider: &NatsToolProvider) -> Result<()> {
    let abort = create_abort_signal();
    let abort_task = {
        let abort = abort.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            abort.set_ctrlc();
        })
    };
    let cancelled = provider
        .call_tool("wait", json!({ "seconds": 30.0 }), &abort)
        .await;

    let in_flight = provider.in_flight_calls();
    let fail_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        in_flight
            .fail_server_unavailable("____time", "time tool process exited")
            .await;
    });
    let failure = provider
        .call_tool("wait", json!({ "seconds": 30.0 }), &create_abort_signal())
        .await;
    fail_task.await?;
    match failure {
        Err(ToolError::Recoverable(error)) => {
            assert_eq!(error.to_string(), "time tool process exited");
        }
        _ => anyhow::bail!("supervisor failure should return a recoverable error"),
    }
    abort_task.await?;
    assert!(matches!(cancelled, Err(ToolError::Fatal(_))));
    Ok(())
}

async fn assert_server_unavailable(
    provider: &NatsToolProvider,
    server_task: tokio::task::JoinHandle<Result<()>>,
) -> Result<()> {
    server_task.abort();
    let _ = server_task.await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let unavailable = provider
        .call_tool(
            "get_current_time",
            json!({ "timezone": "UTC" }),
            &create_abort_signal(),
        )
        .await;
    match unavailable {
        Err(ToolError::Recoverable(error)) => {
            assert!(error.to_string().contains("tool server unavailable"));
        }
        _ => anyhow::bail!("missing server should return a recoverable error"),
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn nats_tool_provider_end_to_end_declarations_cancel_and_precedence() -> Result<()> {
    let Some(server) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(TOKEN.to_string()),
    })
    .await?
    else {
        return Ok(());
    };
    let instance_id = ServerScope::new();
    let _env = EnvGuard::install(server.url(), TOKEN, &instance_id);
    let server_url = server.url.clone();
    let server_instance = instance_id.clone();
    let server_task = tokio::spawn(async move {
        serve_over_nats(TimeToolset::new(), server_instance, &server_url, TOKEN).await
    });
    let workspace = tempfile::tempdir()?;
    let context_task =
        spawn_execution_context_server(instance_id.clone(), server.url.clone(), workspace.path());
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(server.url())
        .await?;
    wait_for_registry(&client, &instance_id, "____time").await?;
    wait_for_registry(&client, &instance_id, "____context").await?;
    set_wait_timeout(&client, &instance_id, 2).await?;
    add_collision_registration(&client, &instance_id).await?;
    add_duplicate_registrations(&client, &instance_id).await?;
    let provider = NatsToolProvider::discover(
        &Config::default(),
        instance_id.clone(),
        NatsInFlightCalls::for_instance(&instance_id),
        None,
    )
    .await?;

    assert_declarations_and_collisions(&provider);
    assert_invocation_and_per_call_timeout(&provider).await?;
    assert_context_flows_from_tool_to_session_enumeration(&provider, &client).await?;

    set_wait_timeout(&client, &instance_id, 1).await?;
    let short_timeout_provider = NatsToolProvider::discover(
        &Config::default(),
        instance_id.clone(),
        NatsInFlightCalls::for_instance(&instance_id),
        None,
    )
    .await?;
    assert_per_call_timeout_enforced(&short_timeout_provider).await?;

    assert_context_declarations_and_precedence(&instance_id).await;
    assert_abort_and_supervisor_failure(&provider).await?;
    let result = assert_server_unavailable(&provider, server_task).await;
    context_task.abort();
    let _ = context_task.await;
    result
}

fn tool_error(error: ToolError) -> anyhow::Error {
    match error {
        ToolError::Recoverable(error) | ToolError::Fatal(error) => error,
    }
}
