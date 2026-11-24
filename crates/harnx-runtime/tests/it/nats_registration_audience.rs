//! Audience-scoped tool registrations: discovery, dispatch and freshness
//! (T2 of the user-credentials plan; decisions 4, 5 and 5b).

use crate::common;
use anyhow::{Context, Result};
use async_trait::async_trait;
use harnx_core::abort::create_abort_signal;
use harnx_core::instance::ServerScope;
use harnx_core::tool::{ToolError, ToolProvider};
use harnx_runtime::config::Config;
use harnx_runtime::nats_tool_provider::{NatsInFlightCalls, NatsToolProvider};
use harnx_runtime::tool_context::discover_nats_tool_provider_cached;
use harnx_toolset::{registration_kv_key, server_identity_token};
use harnx_toolset::{Registration, ToolInvocation, ToolInvokeError, ToolSpec, Toolset};
use harnx_toolset_server::{
    registration_key, serve_over_nats, TOOL_PROTOCOL_VERSION, TOOL_REGISTRY_BUCKET,
    TOOL_SCHEMA_VERSION,
};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TOKEN: &str = "registration-audience-test-token";

/// Set the env a `Config::default()` needs to reach the spawned broker.
/// nextest runs every test in its own process, so this never races.
struct EnvGuard {
    url: Option<String>,
    token: Option<String>,
}

impl EnvGuard {
    // qual:allow(complexity, unsafe) reason: "std::env::set_var is the only way to point Config::default() at the spawned broker; nextest isolates each test process so no other thread reads these vars concurrently."
    fn install(url: &str, token: &str) -> Self {
        let guard = Self {
            url: std::env::var("HARNX_NATS_URL").ok(),
            token: std::env::var("HARNX_NATS_TOKEN").ok(),
        };
        unsafe {
            std::env::set_var("HARNX_NATS_URL", url);
            std::env::set_var("HARNX_NATS_TOKEN", token);
        }
        guard
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: this test is the only thing touching these variables in its
        // process (nextest isolates each test).
        unsafe {
            match self.url.take() {
                Some(value) => std::env::set_var("HARNX_NATS_URL", value),
                None => std::env::remove_var("HARNX_NATS_URL"),
            }
            match self.token.take() {
                Some(value) => std::env::set_var("HARNX_NATS_TOKEN", value),
                None => std::env::remove_var("HARNX_NATS_TOKEN"),
            }
        }
    }
}

/// A tool server that registers only its global management tool. User-scoped
/// and ownerless-only registrations are published beside it as extra
/// registrations (decision 4: same identity, same subject, different
/// audiences), so only the global registration comes from the server itself.
struct IdentityEchoToolset {
    seen_user_ids: Arc<std::sync::Mutex<Vec<Option<String>>>>,
}

impl IdentityEchoToolset {
    fn new() -> (Self, Arc<std::sync::Mutex<Vec<Option<String>>>>) {
        let seen: Arc<std::sync::Mutex<Vec<Option<String>>>> = Arc::default();
        (
            Self {
                seen_user_ids: seen.clone(),
            },
            seen,
        )
    }
}

/// The agent-visible tool name: sanitized server name + raw tool name (see
/// `ServerIdentity::agent_visible_name`), independent of the identity token.
fn visible_tool(server: &str, tool: &str) -> String {
    format!(
        "{}_{tool}",
        harnx_core::package_namespace::sanitize_for_tool_name(server)
    )
}

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        cancellation_guarantee: Default::default(),
        name: name.to_string(),
        description: format!("echo the caller identity for {name}"),
        input_schema: json!({ "type": "object" }),
        idempotent_hint: true,
        read_only_hint: true,
        timeout_secs: Some(30),
        meta: None,
    }
}

#[async_trait]
impl Toolset for IdentityEchoToolset {
    fn name(&self) -> &str {
        "creds"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![spec("manage")]
    }

    async fn invoke(
        &self,
        tool: &str,
        _args: Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        assert_eq!(tool, "manage");
        Err(ToolInvokeError::Recoverable(
            "use invoke_with_context".to_string(),
        ))
    }

    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        let user_id = invocation.context.user_id.clone();
        self.seen_user_ids.lock().unwrap().push(user_id.clone());
        Ok(json!({ "user_id": user_id }))
    }
}

fn registration(server: &str, tools: Vec<ToolSpec>) -> Registration {
    Registration {
        user_id: None,
        ownerless_only: false,
        connection_id: None,
        package: None,
        config: String::new(),
        server: server.to_string(),
        tools,
        schema_version: TOOL_SCHEMA_VERSION,
        proto_version: TOOL_PROTOCOL_VERSION,
    }
}

/// Publish the audience registrations for the `creds` identity: a user-scoped
/// registration for alice (`personal`) and an ownerless-only one (`anon_tool`).
async fn publish_audience_registrations(
    client: &async_nats::Client,
    instance_id: &ServerScope,
) -> Result<()> {
    let store = registry_store(client).await?;
    let mut alice = registration("creds", vec![spec("personal")]);
    alice.user_id = Some("alice".to_string());
    alice.connection_id = Some("conn-1".to_string());
    let mut ownerless = registration("creds", vec![spec("anon_tool")]);
    ownerless.ownerless_only = true;
    ownerless.connection_id = Some("anon-conn".to_string());
    for audience in [alice, ownerless] {
        let key = registration_kv_key(instance_id.as_str(), &audience);
        store
            .put(key, serde_json::to_vec(&audience)?.into())
            .await?;
    }
    Ok(())
}

async fn registry_store(client: &async_nats::Client) -> Result<async_nats::jetstream::kv::Store> {
    Ok(async_nats::jetstream::new(client.clone())
        .get_key_value(TOOL_REGISTRY_BUCKET)
        .await?)
}

/// Publish one user-scoped registration for `user` carrying a single tool.
async fn publish_user_registration(
    client: &async_nats::Client,
    instance_id: &ServerScope,
    user: &str,
    tool: &str,
) -> Result<()> {
    let store = registry_store(client).await?;
    let mut registration = registration("creds", vec![spec(tool)]);
    registration.user_id = Some(user.to_string());
    registration.connection_id = Some(format!("conn-{user}"));
    let key = registration_kv_key(instance_id.as_str(), &registration);
    store
        .put(key, serde_json::to_vec(&registration)?.into())
        .await?;
    Ok(())
}

/// A config whose active agent carries the given session owner — the shape
/// the worker builds for a session turn, so `discover` derives the audience
/// from it exactly as a completion would.
fn config_for_owner(user_id: Option<&str>) -> Config {
    use harnx_runtime::config::agent::Agent;
    let mut config = Config::default();
    let mut agent = Agent::new(
        harnx_core::agent_config::AgentConfig::from_markdown("creds-test", "echo").expect("agent"),
    );
    agent.set_user_principals(Some(harnx_core::user_principals::from_resolved(
        user_id,
        None,
        &[],
        &[],
    )));
    config.agent = Some(agent);
    config
}

async fn discover_for_owner(
    owner: Option<&str>,
    instance_id: &ServerScope,
) -> Result<NatsToolProvider> {
    NatsToolProvider::discover(
        &config_for_owner(owner),
        instance_id.clone(),
        NatsInFlightCalls::for_instance(instance_id),
        None,
    )
    .await
}

async fn wait_for_global_registration(
    client: &async_nats::Client,
    instance_id: &ServerScope,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // The serving task creates the bucket asynchronously; get_key_value
        // 404s until then.
        if let Ok(store) = registry_store(client).await {
            let key = registration_key(instance_id, &server_identity_token(None, "", "creds"));
            if store.get(&key).await?.is_some() {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for the global 'creds' registration");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn call(provider: &NatsToolProvider, name: &str, args: Value) -> Result<Value> {
    let output = provider
        .call_tool(name, args, &create_abort_signal())
        .await
        .map_err(|error| match error {
            ToolError::Recoverable(error) | ToolError::Fatal(error) => error,
        })?;
    anyhow::ensure!(
        output.input_required.is_none(),
        "tool {name} suspended waiting for input"
    );
    Ok(output.value)
}

/// One spawned broker, `creds` server, and client for the tests below.
struct Fixture {
    _server: common::NatsServerHandle,
    _env: EnvGuard,
    instance_id: ServerScope,
    client: async_nats::Client,
    server_task: tokio::task::JoinHandle<Result<()>>,
}

async fn fixture() -> Result<Option<Fixture>> {
    let Some(server) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(TOKEN.to_string()),
    })
    .await?
    else {
        return Ok(None);
    };
    let env = EnvGuard::install(server.url(), TOKEN);
    let instance_id = ServerScope::new();
    let (toolset, _seen) = IdentityEchoToolset::new();
    let server_task = tokio::spawn({
        let url = server.url.clone();
        let instance_id = instance_id.clone();
        async move { serve_over_nats(toolset, instance_id, &url, TOKEN).await }
    });
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(server.url())
        .await?;
    wait_for_global_registration(&client, &instance_id).await?;
    Ok(Some(Fixture {
        _server: server,
        _env: env,
        instance_id,
        client,
        server_task,
    }))
}

/// The shared opening of the owner-visibility tests: publish the audience
/// registrations, then discover Alice's snapshot.
async fn alice_snapshot(fx: &Fixture) -> Result<NatsToolProvider> {
    publish_audience_registrations(&fx.client, &fx.instance_id).await?;
    discover_for_owner(Some("alice"), &fx.instance_id).await
}

/// A user-scoped tool is visible to its owner only; dispatch carries the
/// trusted session owner (never a `user_id` model argument), and neither
/// other users nor ownerless sessions can dispatch it.
#[tokio::test]
async fn user_scoped_registration_is_visible_only_to_its_owner() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fx) = fixture().await? else {
        return Ok(());
    };
    let alice = alice_snapshot(&fx).await?;

    // Alice sees the global management tool and her own user-scoped tool.
    let personal = visible_tool("creds", "personal");
    assert!(alice.has_tool(&personal), "alice sees her user tool");
    let reply = call(&alice, &personal, json!({ "user_id": "bob" })).await?;
    // The worker stamps the session owner; the `user_id` model argument must
    // never override it.
    assert_eq!(reply["user_id"], json!("alice"));

    // Bob sees only the management tool and cannot dispatch Alice's tool.
    let bob = discover_for_owner(Some("bob"), &fx.instance_id).await?;
    assert!(!bob.has_tool(&personal), "bob does not see alice's tool");
    let error = bob
        .call_tool(&personal, json!({}), &create_abort_signal())
        .await
        .expect_err("dispatch from bob's session is rejected");
    let error = match error {
        ToolError::Recoverable(error) | ToolError::Fatal(error) => error,
    };
    assert!(
        error.to_string().contains("not registered"),
        "unexpected rejection: {error:#}"
    );

    // Ownerless sessions don't see user-scoped tools either.
    let ownerless = discover_for_owner(None, &fx.instance_id).await?;
    assert!(
        !ownerless.has_tool(&personal),
        "ownerless session does not see a user-scoped tool"
    );

    fx.server_task.abort();
    let _ = fx.server_task.await;
    Ok(())
}

/// An ownerless-only registration is visible to ownerless sessions only, and
/// dispatch from an owned session is rejected.
#[tokio::test]
async fn ownerless_only_registration_serves_only_ownerless_sessions() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fx) = fixture().await? else {
        return Ok(());
    };
    publish_audience_registrations(&fx.client, &fx.instance_id).await?;

    let anon_tool = visible_tool("creds", "anon_tool");
    let ownerless = discover_for_owner(None, &fx.instance_id).await?;
    assert!(ownerless.has_tool(&anon_tool), "ownerless session sees it");
    let reply = call(&ownerless, &anon_tool, json!({})).await?;
    assert_eq!(reply["user_id"], Value::Null);

    let alice = discover_for_owner(Some("alice"), &fx.instance_id).await?;
    assert!(
        !alice.has_tool(&anon_tool),
        "owned session never sees an ownerless-only tool"
    );
    let error = alice
        .call_tool(&anon_tool, json!({}), &create_abort_signal())
        .await
        .expect_err("dispatch from an owned session is rejected");
    let error = match error {
        ToolError::Recoverable(error) | ToolError::Fatal(error) => error,
    };
    assert!(
        error.to_string().contains("not registered"),
        "unexpected rejection: {error:#}"
    );

    fx.server_task.abort();
    let _ = fx.server_task.await;
    Ok(())
}

/// Alice calls both the global management tool and her user tool from one
/// snapshot (union semantics per identity); Bob gets only the management tool.
#[tokio::test]
async fn union_routing_spans_global_and_user_registrations() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fx) = fixture().await? else {
        return Ok(());
    };
    let alice = alice_snapshot(&fx).await?;
    for tool in ["manage", "personal"] {
        let name = visible_tool("creds", tool);
        assert!(alice.has_tool(&name), "alice routes {name}");
        let reply = call(&alice, &name, json!({})).await?;
        assert_eq!(reply["user_id"], json!("alice"));
    }

    let bob = discover_for_owner(Some("bob"), &fx.instance_id).await?;
    assert!(bob.has_tool(&visible_tool("creds", "manage")));
    assert!(!bob.has_tool(&visible_tool("creds", "personal")));

    fx.server_task.abort();
    let _ = fx.server_task.await;
    Ok(())
}

/// A written or deleted user registration shows up on the very next cached
/// discovery, without waiting out the 30 s cache interval (decision 5b).
#[tokio::test]
async fn next_discovery_sees_written_and_deleted_registrations() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fx) = fixture().await? else {
        return Ok(());
    };
    let config = config_for_owner(Some("alice"));
    let personal = visible_tool("creds", "personal");

    let before = discover_nats_tool_provider_cached(&config, &fx.instance_id, None)
        .await
        .context("first discovery")?;
    assert!(
        !before.has_tool(&personal),
        "the user tool is not registered yet"
    );

    // The `<server>_connect` path: the registration is written, the call
    // returns, and the very next completion runs immediately. No sleep, no
    // waiting on the async watch — the deterministic freshness check compares
    // the registry stream's sequence against the one the cached snapshot
    // started scanning under, and the write ack bumps it before this returns.
    publish_audience_registrations(&fx.client, &fx.instance_id).await?;
    let after_write = discover_nats_tool_provider_cached(&config, &fx.instance_id, None)
        .await
        .context("discovery after write")?;
    assert!(
        !Arc::ptr_eq(&before, &after_write),
        "the cached snapshot must be invalidated by the write itself"
    );
    assert!(after_write.has_tool(&personal), "next completion sees it");

    // The disconnect path: the delete acks, the next completion runs
    // immediately, and the tool is gone.
    let store = registry_store(&fx.client).await?;
    let mut alice_registration = registration("creds", vec![spec("personal")]);
    alice_registration.user_id = Some("alice".to_string());
    let key = registration_kv_key(fx.instance_id.as_str(), &alice_registration);
    store.delete(&key).await?;
    let after_delete = discover_nats_tool_provider_cached(&config, &fx.instance_id, None)
        .await
        .context("discovery after delete")?;
    assert!(
        !after_delete.has_tool(&personal),
        "the next completion after a delete excludes the tool"
    );

    fx.server_task.abort();
    let _ = fx.server_task.await;
    Ok(())
}

/// An unchanged registry must reuse the cached provider — same `Arc` on the
/// second lookup. The reuse keeps a cache hit from paying a fresh discovery
/// (and with it a fresh connection); it is not by itself a connection-count
/// guard, since HEAD also returned the same provider — only the pinned
/// latency test plus this test together catch a per-lookup connection
/// regression.
#[tokio::test]
async fn nats_cached_discovery_reuses_provider_while_registry_unchanged() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fx) = fixture().await? else {
        return Ok(());
    };
    // The serving task renews its registration KV entry on a fixed interval;
    // stop it so no renewal can land between the two lookups below and
    // legitimately invalidate the first provider.
    fx.server_task.abort();
    let _ = fx.server_task.await;

    let config = config_for_owner(Some("alice"));
    let first = discover_nats_tool_provider_cached(&config, &fx.instance_id, None)
        .await
        .context("first discovery")?;
    let second = discover_nats_tool_provider_cached(&config, &fx.instance_id, None)
        .await
        .context("second discovery")?;

    assert!(
        Arc::ptr_eq(&first, &second),
        "an unchanged registry must reuse the cached provider"
    );
    assert!(
        first.has_tool(&visible_tool("creds", "manage")),
        "the reused snapshot still sees the global registration"
    );
    Ok(())
}

/// Deleting the whole registry bucket invalidates the cached snapshot on the
/// very next lookup even though the KV watch never fires a bucket-deleted
/// event: the deleted stream reads as sequence 0, which no snapshot cached
/// with a later revision can match (the freshness check the probe's
/// `Some(0)` mapping exists for). The epoch is asserted unchanged across
/// the deletion so this cannot pass vacuously by an epoch bump.
#[tokio::test]
async fn nats_deleting_the_registry_bucket_invalidates_cached_discovery() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fx) = fixture().await? else {
        return Ok(());
    };
    let global = visible_tool("creds", "manage");
    let config = config_for_owner(Some("alice"));
    let before = discover_nats_tool_provider_cached(&config, &fx.instance_id, None)
        .await
        .context("discovery before deletion")?;
    assert!(before.has_tool(&global), "the global tool is registered");
    let epoch_before = harnx_runtime::tool_registry_watch::registry_epoch();

    // Stop the serving task first so its periodic renewal cannot recreate
    // the bucket or race the deletion below.
    fx.server_task.abort();
    let _ = fx.server_task.await;
    let jetstream = async_nats::jetstream::new(fx.client.clone());
    jetstream
        .delete_key_value(TOOL_REGISTRY_BUCKET)
        .await
        .context("delete the whole registry bucket")?;

    let epoch_after = harnx_runtime::tool_registry_watch::registry_epoch();
    assert_eq!(
        epoch_before, epoch_after,
        "the watch must not fire for a deleted bucket; only the revision check invalidates"
    );
    let after = discover_nats_tool_provider_cached(&config, &fx.instance_id, None)
        .await
        .context("discovery after bucket deletion")?;
    assert!(
        !Arc::ptr_eq(&before, &after),
        "the cached snapshot must not survive bucket deletion"
    );
    assert!(
        !after.has_tool(&global),
        "removed registrations must not stay advertised until the TTL"
    );
    Ok(())
}

/// Decision 11: an operator call acts for the operator's own identity, not
/// the active session owner's. The worker re-stamps the config's active
/// agent with the operator identity before dispatch
/// (`nats_worker/operator_tools.rs`); this reproduces that path and checks
/// discovery and dispatch follow it.
#[tokio::test]
async fn operator_identity_replaces_the_session_owner() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fx) = fixture().await? else {
        return Ok(());
    };
    // One user-scoped registration each for the session owner (alice) and
    // the operator (bob).
    publish_user_registration(&fx.client, &fx.instance_id, "alice", "personal").await?;
    publish_user_registration(&fx.client, &fx.instance_id, "bob", "ops").await?;

    let mut config = config_for_owner(Some("alice"));
    let alice = discover_nats_tool_provider_cached(&config, &fx.instance_id, None)
        .await
        .context("discovery as the session owner")?;
    assert!(alice.has_tool(&visible_tool("creds", "personal")));
    assert!(!alice.has_tool(&visible_tool("creds", "ops")));
    let reply = call(&alice, &visible_tool("creds", "personal"), json!({})).await?;
    assert_eq!(reply["user_id"], json!("alice"));

    // The operator call: the worker stamps the operator's identity over the
    // session agent, exactly as `OperatorToolRequest::user_id` asks it to.
    config.apply_session_agent_patches(
        &harnx_core::user_principals::from_resolved(Some("bob"), None, &[], &[]),
        &harnx_core::agent_config::AgentVariables::default(),
    )?;
    let operator = discover_nats_tool_provider_cached(&config, &fx.instance_id, None)
        .await
        .context("discovery as the operator")?;
    assert!(
        !operator.has_tool(&visible_tool("creds", "personal")),
        "the operator does not inherit the session owner's user tools"
    );
    assert!(operator.has_tool(&visible_tool("creds", "ops")));
    let reply = call(&operator, &visible_tool("creds", "ops"), json!({})).await?;
    assert_eq!(reply["user_id"], json!("bob"));

    fx.server_task.abort();
    let _ = fx.server_task.await;
    Ok(())
}

/// Registrations filed under the wrong audience key are ignored — the exact
/// rule a pre-change worker enforced for `.u.{U}` and `.anon` keys, kept
/// identical for everything that isn't one of the two new suffixes.
#[tokio::test]
async fn registrations_under_wrong_audience_keys_are_ignored() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fx) = fixture().await? else {
        return Ok(());
    };
    let store = registry_store(&fx.client).await?;
    let instance_id = &fx.instance_id;

    // A user-scoped payload under the bare global key of a *different* server
    // (so the live `creds` registration stays untouched): the recomputed key
    // is the `.u.{U}` one, so this is ignored for everyone — the same outcome
    // the pre-change exact-match rule produced.
    let mut misfiled_global = registration("misfiled", vec![spec("manage")]);
    misfiled_global.user_id = Some("alice".to_string());
    let bare_key = registration_key(instance_id, &server_identity_token(None, "", "misfiled"));
    store
        .put(bare_key, serde_json::to_vec(&misfiled_global)?.into())
        .await?;

    // A global payload under a `.u.{U}` key is ignored as well: the
    // recomputed key is the bare one.
    let user_keyed = registration("misfiled-user", vec![spec("manage")]);
    let mut user_scoped = registration("misfiled-user", vec![spec("personal")]);
    user_scoped.user_id = Some("alice".to_string());
    let user_key = registration_kv_key(instance_id.as_str(), &user_scoped);
    assert_eq!(
        user_key,
        format!(
            "{instance_id}.{}.u.{}",
            server_identity_token(None, "", "misfiled-user"),
            harnx_toolset::user_key_segment("alice")
        )
    );
    store
        .put(user_key, serde_json::to_vec(&user_keyed)?.into())
        .await?;

    // An unrecognized suffix is ignored by both old and new rules.
    let unrecognized = registration("misc", vec![spec("unrecognized")]);
    store
        .put(
            format!(
                "{instance_id}.{}.misc",
                server_identity_token(None, "", "misc")
            ),
            serde_json::to_vec(&unrecognized)?.into(),
        )
        .await?;

    let alice = discover_for_owner(Some("alice"), &fx.instance_id).await?;
    for (server, tool) in [
        ("misfiled", "manage"),
        ("misfiled-user", "personal"),
        ("misc", "unrecognized"),
    ] {
        let name = visible_tool(server, tool);
        assert!(!alice.has_tool(&name), "ignored: {name}");
    }
    // The live global registration is unaffected.
    assert!(alice.has_tool(&visible_tool("creds", "manage")));

    fx.server_task.abort();
    let _ = fx.server_task.await;
    Ok(())
}
