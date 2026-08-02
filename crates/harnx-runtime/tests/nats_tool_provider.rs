#[allow(dead_code)]
mod common;

use anyhow::Result;
use harnx_core::abort::create_abort_signal;
use harnx_core::instance::InstanceId;
use harnx_core::tool::{ToolError, ToolProvider};
use harnx_runtime::config::{Config, HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV};
use harnx_runtime::nats_tool_provider::{NatsInFlightCalls, NatsToolProvider};
use harnx_time_server::TimeToolset;
use harnx_toolset::{Registration, ToolSpec};
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

struct EnvGuard {
    url: Option<OsString>,
    token: Option<OsString>,
}

impl EnvGuard {
    fn install(url: &str, token: &str) -> Self {
        let guard = Self {
            url: std::env::var_os(HARNX_NATS_URL_ENV),
            token: std::env::var_os(HARNX_NATS_TOKEN_ENV),
        };
        unsafe {
            std::env::set_var(HARNX_NATS_URL_ENV, url);
            std::env::set_var(HARNX_NATS_TOKEN_ENV, token);
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
        }
    }
}

async fn wait_for_registry(client: &async_nats::Client, instance_id: &InstanceId) -> Result<()> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(store) = jetstream.get_key_value(TOOL_REGISTRY_BUCKET).await {
            if store
                .get(registration_key(instance_id, "time"))
                .await?
                .is_some()
            {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for time tool registration");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn set_wait_timeout(
    client: &async_nats::Client,
    instance_id: &InstanceId,
    timeout_secs: u64,
) -> Result<()> {
    let store = async_nats::jetstream::new(client.clone())
        .get_key_value(TOOL_REGISTRY_BUCKET)
        .await?;
    let key = registration_key(instance_id, "time");
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
    instance_id: &InstanceId,
) -> Result<()> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let store = jetstream.get_key_value(TOOL_REGISTRY_BUCKET).await?;
    let registration = Registration {
        server: "collision".to_string(),
        tools: vec![ToolSpec {
            name: harnx_runtime::session_history::TOOL_NAME.to_string(),
            description: "NATS collision test".to_string(),
            input_schema: json!({ "type": "object" }),
            idempotent_hint: true,
            read_only_hint: true,
            timeout_secs: None,
        }],
        schema_version: TOOL_SCHEMA_VERSION,
        proto_version: TOOL_PROTOCOL_VERSION,
    };
    store
        .put(
            registration_key(instance_id, &registration.server),
            serde_json::to_vec(&registration)?.into(),
        )
        .await?;
    Ok(())
}

async fn add_duplicate_registrations(
    client: &async_nats::Client,
    instance_id: &InstanceId,
) -> Result<()> {
    let store = async_nats::jetstream::new(client.clone())
        .get_key_value(TOOL_REGISTRY_BUCKET)
        .await?;
    for server in ["alpha", "beta"] {
        let registration = Registration {
            server: server.to_string(),
            tools: vec![ToolSpec {
                name: "duplicate_tool".to_string(),
                description: format!("owned by {server}"),
                input_schema: json!({ "type": "object" }),
                idempotent_hint: true,
                read_only_hint: true,
                timeout_secs: Some(30),
            }],
            schema_version: TOOL_SCHEMA_VERSION,
            proto_version: TOOL_PROTOCOL_VERSION,
        };
        store
            .put(
                registration_key(instance_id, server),
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
            .filter(|tool| tool.name == "duplicate_tool")
            .count(),
        1
    );
    assert!(provider
        .declarations_for_use_tools(Some("beta"))
        .iter()
        .any(|tool| tool.name == "duplicate_tool"));
    assert!(!provider
        .declarations_for_use_tools(Some("alpha"))
        .iter()
        .any(|tool| tool.name == "duplicate_tool"));
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

async fn assert_context_declarations_and_precedence(instance_id: &InstanceId) {
    let config = Arc::new(RwLock::new(Config::default()));
    let context = harnx_runtime::tool::build_tool_eval_context(
        harnx_runtime::tool::BuildToolEvalContextParams::new(&config, instance_id)
            .with_agent_use_tools(Some("*")),
    )
    .await;
    let declarations = &context.render.as_ref().expect("render context").decl_map;
    for tool in ["get_current_time", "convert_time", "wait", "wait_until"] {
        assert!(
            declarations.contains_key(tool),
            "pilot tool not declared: {tool}"
        );
        assert!(context.allowed_tool_names.contains(tool));
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
            .fail_server_unavailable("time", "time tool process exited")
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
    let _env = EnvGuard::install(server.url(), TOKEN);
    let instance_id = InstanceId::new();
    let server_url = server.url.clone();
    let server_instance = instance_id.clone();
    let server_task = tokio::spawn(async move {
        serve_over_nats(TimeToolset::new(), server_instance, &server_url, TOKEN).await
    });
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(server.url())
        .await?;
    wait_for_registry(&client, &instance_id).await?;
    set_wait_timeout(&client, &instance_id, 2).await?;
    add_collision_registration(&client, &instance_id).await?;
    add_duplicate_registrations(&client, &instance_id).await?;
    let provider = NatsToolProvider::discover(
        &Config::default(),
        instance_id.clone(),
        NatsInFlightCalls::for_instance(&instance_id),
    )
    .await?;

    assert_declarations_and_collisions(&provider);
    assert_invocation_and_per_call_timeout(&provider).await?;

    set_wait_timeout(&client, &instance_id, 1).await?;
    let short_timeout_provider = NatsToolProvider::discover(
        &Config::default(),
        instance_id.clone(),
        NatsInFlightCalls::for_instance(&instance_id),
    )
    .await?;
    assert_per_call_timeout_enforced(&short_timeout_provider).await?;

    assert_context_declarations_and_precedence(&instance_id).await;
    assert_abort_and_supervisor_failure(&provider).await?;
    assert_server_unavailable(&provider, server_task).await
}

fn tool_error(error: ToolError) -> anyhow::Error {
    match error {
        ToolError::Recoverable(error) | ToolError::Fatal(error) => error,
    }
}
