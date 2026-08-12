#[allow(dead_code)]
mod common;

use anyhow::{Context, Result};
use harnx_core::hooks::{HookConfig, HookEvent, HookResultControl, HooksConfig};
use harnx_core::instance::ServerScope;
use harnx_hookset::{FailPolicy, HookRegistration, HOOK_EXPECTATIONS_BUCKET, HOOK_REGISTRY_BUCKET};
use harnx_hookset_server::hook_registration_key;
use harnx_runtime::nats_hook_provider::{HookDispatchMeta, NatsHookProvider};
use harnx_runtime::nats_worker::{
    publish_crash_rejector, reconcile_hook_supervisor, HookServerStartConfig, HookServerSupervisor,
    RejectorTarget,
};
use serde_json::json;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
struct RequestId<'a>(&'a str);

impl<'a> From<&'a str> for RequestId<'a> {
    fn from(value: &'a str) -> Self {
        Self(value)
    }
}
const TOKEN: &str = "hook-supervisor-test-token";

fn hook_config(event: &str) -> HooksConfig {
    hook_config_with_command(event, &["cat"])
}

fn hook_config_with_command(event: &str, child_argv: &[&str]) -> HooksConfig {
    HooksConfig {
        max_resume: None,
        entries: vec![HookConfig {
            command: format!(
                "harnx-claude-compatible-hook-server --event {} --matcher exec --timeout 5 -- {}",
                shell_words::quote(event),
                shell_words::join(child_argv.iter().copied())
            ),
            status_message: None,
            async_hook: None,
            package_dir: None,
        }],
    }
}

fn ensure_hook_server_binary() -> Result<PathBuf> {
    let mut path = std::env::current_exe().context("resolve test executable")?;
    path.pop();
    if path.file_name().is_some_and(|name| name == "deps") {
        path.pop();
    }
    path.push(if cfg!(windows) {
        "harnx-claude-compatible-hook-server.exe"
    } else {
        "harnx-claude-compatible-hook-server"
    });
    if path.is_file() {
        return Ok(path);
    }
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .context("resolve workspace root")?
        .to_path_buf();
    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["build", "-p", "harnx-claude-compatible-hook-server"])
        .current_dir(workspace)
        .status()
        .context("build hook server for supervisor test")?;
    anyhow::ensure!(status.success(), "building hook server failed");
    anyhow::ensure!(
        path.is_file(),
        "hook server not found at {}",
        path.display()
    );
    Ok(path)
}

struct TestNatsContext {
    _server: common::NatsServerHandle,
    client: async_nats::Client,
    instance_id: ServerScope,
    start: HookServerStartConfig,
}

async fn test_nats_context() -> Result<Option<TestNatsContext>> {
    ensure_hook_server_binary()?;
    let Some(server) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(TOKEN.to_string()),
    })
    .await?
    else {
        return Ok(None);
    };
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(server.url())
        .await?;
    let instance_id = ServerScope::new();
    let start =
        HookServerStartConfig::new(client.clone(), instance_id.clone(), server.url(), TOKEN);
    Ok(Some(TestNatsContext {
        _server: server,
        client,
        instance_id,
        start,
    }))
}

async fn create_store(
    client: &async_nats::Client,
    bucket: &str,
) -> Result<async_nats::jetstream::kv::Store> {
    Ok(async_nats::jetstream::new(client.clone())
        .create_key_value(async_nats::jetstream::kv::Config {
            bucket: bucket.to_string(),
            ..Default::default()
        })
        .await?)
}

async fn dispatch_pre_tool(
    client: &async_nats::Client,
    instance_id: &ServerScope,
    request_id: RequestId<'_>,
) -> Result<harnx_core::hooks::HookOutcome> {
    dispatch_event(
        client,
        instance_id,
        request_id,
        HookEvent::PreToolUse {
            tool_name: "exec".to_string(),
            tool_input: json!({"command": "true"}),
            tool_use_id: request_id.0.to_string(),
        },
    )
    .await
}

async fn dispatch_user_prompt(
    client: &async_nats::Client,
    instance_id: &ServerScope,
    request_id: RequestId<'_>,
) -> Result<harnx_core::hooks::HookOutcome> {
    dispatch_event(
        client,
        instance_id,
        request_id,
        HookEvent::UserPromptSubmit {
            prompt: "hello".to_string(),
        },
    )
    .await
}

async fn dispatch_event(
    client: &async_nats::Client,
    instance_id: &ServerScope,
    request_id: RequestId<'_>,
    event: HookEvent,
) -> Result<harnx_core::hooks::HookOutcome> {
    let provider =
        NatsHookProvider::discover_with_client(client.clone(), instance_id.clone()).await?;
    Ok(provider
        .dispatch_event(
            event,
            None,
            HookDispatchMeta {
                session_id: request_id.0.to_string(),
                cwd: std::env::current_dir()?,
                resume_count: 0,
            },
        )
        .await)
}

async fn wait_until_removed(store: &async_nats::jetstream::kv::Store, key: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if store.get(key).await?.is_none() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("hook registration '{key}' was not removed");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn run_scope_transitions(
    start: &HookServerStartConfig,
    store: &async_nats::jetstream::kv::Store,
    instance_id: &ServerScope,
) -> Result<()> {
    let scopes = [
        ("global", "PreToolUse"),
        ("tool-time", "PostToolUse"),
        ("session-old", "SessionStart"),
        ("session-new", "Stop"),
    ];
    let mut active: Option<HookServerSupervisor> = None;
    for (scope, event) in scopes {
        if let Some(mut previous) = active.take() {
            let previous_name = previous
                .server_pids()
                .await
                .into_values()
                .next()
                .context("previous hook process")?;
            let previous_key = hook_registration_key(instance_id, &previous_name);
            previous.shutdown().await;
            wait_until_removed(store, &previous_key).await?;
        }
        let supervisor = HookServerSupervisor::start_local_with_timeout(
            start.clone(),
            &hook_config(event),
            scope,
            Duration::from_secs(5),
        )
        .await?;
        let name = supervisor
            .server_pids()
            .await
            .into_values()
            .next()
            .context("hook process")?;
        let key = hook_registration_key(instance_id, &name);
        assert!(store.get(&key).await?.is_some(), "missing {scope} hook");
        active = Some(supervisor);
    }
    let mut final_supervisor = active.context("final hook supervisor")?;
    let final_name = final_supervisor
        .server_pids()
        .await
        .into_values()
        .next()
        .context("final hook process")?;
    let final_key = hook_registration_key(instance_id, &final_name);
    final_supervisor.shutdown().await;
    wait_until_removed(store, &final_key).await
}

async fn wait_for_registration_value(
    store: &async_nats::jetstream::kv::Store,
    key: &str,
) -> Result<HookRegistration> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(value) = store.get(key).await? {
            return serde_json::from_slice(&value).context("decode expected hook registration");
        }
        if Instant::now() >= deadline {
            anyhow::bail!("hook registration '{key}' was not published");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn scoped_hooks_register_and_unregister_across_lifecycle_transitions() -> Result<()> {
    harnx_core::require_nextest();
    let Some(context) = test_nats_context().await? else {
        return Ok(());
    };
    let TestNatsContext {
        client,
        instance_id,
        start,
        _server,
    } = context;

    let store = create_store(&client, HOOK_REGISTRY_BUCKET).await?;

    run_scope_transitions(&start, &store, &instance_id).await
}

#[tokio::test(flavor = "multi_thread")]
async fn reconcile_hook_supervisor_replaces_old_agent_registration() -> Result<()> {
    harnx_core::require_nextest();
    let Some(context) = test_nats_context().await? else {
        return Ok(());
    };
    let TestNatsContext {
        client,
        instance_id,
        start,
        _server,
    } = context;
    let store = create_store(&client, HOOK_REGISTRY_BUCKET).await?;

    let old_supervisor = HookServerSupervisor::start_local_with_timeout(
        start.clone(),
        &hook_config("SessionStart"),
        "session-old-agent",
        Duration::from_secs(5),
    )
    .await?;
    let old_name = old_supervisor
        .server_pids()
        .await
        .into_values()
        .next()
        .context("old-agent hook process")?;
    let old_key = hook_registration_key(&instance_id, &old_name);
    assert!(store.get(&old_key).await?.is_some());

    let mut active = Some(old_supervisor);
    reconcile_hook_supervisor(
        &mut active,
        Some(&start),
        &hook_config("Stop"),
        "session-new-agent",
    )
    .await;

    wait_until_removed(&store, &old_key).await?;
    let mut new_supervisor = active.context("new-agent hook supervisor")?;
    let new_name = new_supervisor
        .server_pids()
        .await
        .into_values()
        .next()
        .context("new-agent hook process")?;
    let new_key = hook_registration_key(&instance_id, &new_name);
    assert_ne!(old_key, new_key);
    assert!(store.get(&new_key).await?.is_some());

    new_supervisor.shutdown().await;
    wait_until_removed(&store, &new_key).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_hook_start_installs_fail_closed_rejector() -> Result<()> {
    harnx_core::require_nextest();
    let Some(context) = test_nats_context().await? else {
        return Ok(());
    };
    let TestNatsContext {
        client,
        instance_id,
        start,
        _server,
    } = context;
    let mut hooks = hook_config("PreToolUse");
    hooks.entries[0].command = "harnx-proxy-auth --hook 'unterminated".to_string();
    hooks.entries[0].status_message = Some("Friendly failure guard".to_string());

    let mut supervisor = HookServerSupervisor::start_local_with_timeout(
        start,
        &hooks,
        "tool-bash",
        Duration::from_millis(100),
    )
    .await?;
    assert!(supervisor.server_pids().await.is_empty());

    let store = async_nats::jetstream::new(client.clone())
        .get_key_value(HOOK_EXPECTATIONS_BUCKET)
        .await?;
    let mut keys = store.keys().await?;
    use futures_util::TryStreamExt as _;
    let prefix = format!("{instance_id}.hook-");
    let mut rejector_key = None;
    while let Some(key) = keys.try_next().await? {
        if key.starts_with(&prefix) && key.ends_with("-rejector") {
            rejector_key = Some(key);
            break;
        }
    }
    let key = rejector_key.context("startup rejector key")?;
    let rejector = wait_for_registration_value(&store, &key).await?;
    assert_eq!(key, hook_registration_key(&instance_id, &rejector.server));
    assert!(rejector.server.ends_with("-rejector"));
    assert_eq!(
        rejector.display_label.as_deref(),
        Some("hook server failed to start: Friendly failure guard")
    );
    assert_eq!(rejector.hooks.len(), 2);
    assert_eq!(rejector.hooks[0].event, "UserPromptSubmit");
    assert!(rejector.hooks[0].matcher.is_none());
    assert_eq!(rejector.hooks[0].fail_policy, FailPolicy::Closed);
    assert_eq!(rejector.hooks[1].event, "PreToolUse");
    assert_eq!(rejector.hooks[1].matcher.as_deref(), Some(".*"));
    assert_eq!(rejector.hooks[1].fail_policy, FailPolicy::Closed);

    let expected_reason = HookResultControl::Block {
        reason: "hook server failed to start: Friendly failure guard hook unavailable".to_string(),
    };
    assert_eq!(
        dispatch_pre_tool(&client, &instance_id, "failed-start".into())
            .await?
            .control,
        expected_reason
    );
    assert_eq!(
        dispatch_user_prompt(&client, &instance_id, "failed-prompt".into())
            .await?
            .control,
        expected_reason
    );

    supervisor.shutdown().await;
    wait_until_removed(&store, &key).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_failure_blocks_user_prompt_with_friendly_label() -> Result<()> {
    harnx_core::require_nextest();
    let Some(context) = test_nats_context().await? else {
        return Ok(());
    };
    let TestNatsContext {
        client,
        instance_id,
        start,
        _server,
    } = context;
    let mut hooks = hook_config("PreToolUse");
    hooks.entries[0].command = "true".to_string();
    hooks.entries[0].status_message = Some("Prompt safety hook".to_string());

    let _supervisor = HookServerSupervisor::start_local_with_timeout(
        start,
        &hooks,
        "prompt-failure",
        Duration::from_secs(2),
    )
    .await?;
    assert_eq!(
        dispatch_user_prompt(&client, &instance_id, "failed-prompt-label".into())
            .await?
            .control,
        HookResultControl::Block {
            reason: "hook server failed to start: Prompt safety hook hook unavailable".to_string(),
        }
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn readiness_timeout_installs_rejector_and_blocks_gate_events() -> Result<()> {
    harnx_core::require_nextest();
    let Some(context) = test_nats_context().await? else {
        return Ok(());
    };
    let TestNatsContext {
        client,
        instance_id,
        start,
        _server,
    } = context;
    let mut hooks = hook_config("PreToolUse");
    hooks.entries[0].command = "sh -c 'sleep 30'".to_string();
    hooks.entries[0].status_message = Some("Timed out safety guard".to_string());

    let started = Instant::now();
    let mut supervisor = HookServerSupervisor::start_local_with_timeout(
        start,
        &hooks,
        "timeout-test",
        Duration::from_millis(100),
    )
    .await?;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(supervisor.server_pids().await.is_empty());

    let store = async_nats::jetstream::new(client.clone())
        .get_key_value(HOOK_EXPECTATIONS_BUCKET)
        .await?;
    let mut keys = store.keys().await?;
    use futures_util::TryStreamExt as _;
    let prefix = format!("{instance_id}.hook-");
    let mut rejector_key = None;
    while let Some(key) = keys.try_next().await? {
        if key.starts_with(&prefix) && key.ends_with("-rejector") {
            rejector_key = Some(key);
            break;
        }
    }
    let key = rejector_key.context("timeout rejector key")?;
    let rejector = wait_for_registration_value(&store, &key).await?;
    assert_eq!(
        rejector.display_label.as_deref(),
        Some("hook server failed to start: Timed out safety guard")
    );

    let expected = HookResultControl::Block {
        reason: "hook server failed to start: Timed out safety guard hook unavailable".to_string(),
    };
    assert_eq!(
        dispatch_pre_tool(&client, &instance_id, "timeout-pre-tool".into())
            .await?
            .control,
        expected
    );
    assert_eq!(
        dispatch_user_prompt(&client, &instance_id, "timeout-prompt".into())
            .await?
            .control,
        expected
    );

    supervisor.shutdown().await;
    wait_until_removed(&store, &key).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn crashed_closed_hook_blocks_through_retained_expectation() -> Result<()> {
    harnx_core::require_nextest();
    let Some(context) = test_nats_context().await? else {
        return Ok(());
    };
    let TestNatsContext {
        client,
        instance_id,
        start,
        _server,
    } = context;
    let mut hooks = hook_config("PreToolUse");
    // Server flags must precede `--`; anything after it belongs to the child argv.
    hooks.entries[0].command = hooks.entries[0]
        .command
        .replace(" -- ", " --fail-policy open -- ");
    let supervisor = HookServerSupervisor::start_local_with_timeout(
        start,
        &hooks,
        "crash-test",
        Duration::from_secs(5),
    )
    .await?;
    let (pid, name) = supervisor
        .server_pids()
        .await
        .into_iter()
        .next()
        .context("healthy hook process")?;
    let registry = async_nats::jetstream::new(client.clone())
        .get_key_value(HOOK_REGISTRY_BUCKET)
        .await?;
    let key = hook_registration_key(&instance_id, &name);
    let live = registry
        .get(&key)
        .await?
        .context("live hook registration")?;
    let live: HookRegistration = serde_json::from_slice(&live)?;
    assert!(live
        .hooks
        .iter()
        .all(|hook| hook.fail_policy == FailPolicy::Open));

    // SAFETY: pid belongs to child process owned by this test's supervisor.
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("SIGKILL hook server");
    }
    wait_until_removed(&registry, &key).await?;
    let expectations = async_nats::jetstream::new(client.clone())
        .get_key_value(HOOK_EXPECTATIONS_BUCKET)
        .await?;
    let marker = wait_for_registration_value(&expectations, &key).await?;
    assert_eq!(marker.server, name);
    assert!(!marker.hooks.is_empty());
    assert!(marker
        .hooks
        .iter()
        .all(|hook| hook.fail_policy == FailPolicy::Closed));

    let outcome = dispatch_pre_tool(&client, &instance_id, "crashed-hook".into()).await?;
    assert!(matches!(outcome.control, HookResultControl::Block { .. }));
    drop(supervisor);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn healthy_closed_hook_with_expectation_dispatches_normally() -> Result<()> {
    harnx_core::require_nextest();
    let Some(context) = test_nats_context().await? else {
        return Ok(());
    };
    let TestNatsContext {
        client,
        instance_id,
        start,
        _server,
    } = context;
    let hooks = hook_config_with_command("PreToolUse", &["printf", "{}"]);
    let supervisor = HookServerSupervisor::start_local_with_timeout(
        start,
        &hooks,
        "healthy-test",
        Duration::from_secs(5),
    )
    .await?;

    let provider = NatsHookProvider::discover_with_client(client, instance_id).await?;
    assert_eq!(
        provider.hooks().len(),
        1,
        "expectation duplicated live hook"
    );
    let outcome = provider
        .dispatch_event(
            HookEvent::PreToolUse {
                tool_name: "exec".to_string(),
                tool_input: json!({"command": "true"}),
                tool_use_id: "healthy-hook".to_string(),
            },
            None,
            HookDispatchMeta {
                session_id: "healthy-hook".to_string(),
                cwd: std::env::current_dir()?,
                resume_count: 0,
            },
        )
        .await;
    assert_eq!(outcome.control, HookResultControl::Continue);
    drop(supervisor);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_rejector_falls_back_to_registry_and_blocks_gate_events() -> Result<()> {
    harnx_core::require_nextest();
    let Some(context) = test_nats_context().await? else {
        return Ok(());
    };
    let TestNatsContext {
        client,
        instance_id,
        _server,
        ..
    } = context;

    // Force the expectations publish to exceed the bucket limit while leaving
    // the registry bucket available for the second publication attempt.
    let expectations = async_nats::jetstream::new(client.clone())
        .create_key_value(async_nats::jetstream::kv::Config {
            bucket: HOOK_EXPECTATIONS_BUCKET.to_string(),
            max_bytes: 1,
            ..Default::default()
        })
        .await?;
    let crashed_server = "hook-direct-crash";
    let rejector_server = format!("{crashed_server}-rejector");
    let friendly_label = "hook server crashed: Direct fallback guard";
    let key = hook_registration_key(&instance_id, &rejector_server);

    publish_crash_rejector(
        &client,
        &instance_id,
        RejectorTarget {
            server: &rejector_server,
            display_label: friendly_label,
        },
    )
    .await?;

    assert!(
        expectations.get(&key).await?.is_none(),
        "rejector unexpectedly landed in constrained expectations bucket"
    );
    let registry = async_nats::jetstream::new(client.clone())
        .get_key_value(HOOK_REGISTRY_BUCKET)
        .await?;
    let rejector = wait_for_registration_value(&registry, &key).await?;
    assert_eq!(rejector.server, rejector_server);
    assert_eq!(rejector.display_label.as_deref(), Some(friendly_label));

    let expected = HookResultControl::Block {
        reason: format!("{friendly_label} hook unavailable"),
    };
    assert_eq!(
        dispatch_user_prompt(&client, &instance_id, "crash-rejector-prompt".into())
            .await?
            .control,
        expected
    );
    assert_eq!(
        dispatch_pre_tool(&client, &instance_id, "crash-rejector-tool".into())
            .await?
            .control,
        expected
    );
    Ok(())
}
