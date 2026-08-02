#[allow(dead_code)]
mod common;

use anyhow::{Context, Result};
use harnx_core::hooks::{HookConfig, HookEvent, HookResultControl, HooksConfig};
use harnx_core::instance::InstanceId;
use harnx_hookset::{HOOK_EXPECTATIONS_BUCKET, HOOK_REGISTRY_BUCKET};
use harnx_hookset_server::hook_registration_key;
use harnx_runtime::nats_hook_provider::{HookDispatchMeta, NatsHookProvider};
use harnx_runtime::nats_worker::{
    reconcile_hook_supervisor, HookServerStartConfig, HookServerSupervisor,
};
use serde_json::json;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

const TOKEN: &str = "hook-supervisor-test-token";

fn hook_config(event: &str) -> HooksConfig {
    HooksConfig {
        max_resume: None,
        entries: vec![HookConfig {
            event: event.to_string(),
            matcher: Some("exec".to_string()),
            command: "cat".to_string(),
            timeout: Some(5),
            status_message: None,
            async_hook: None,
            hook_type: "claude-command".to_string(),
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
    instance_id: InstanceId,
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
    let instance_id = InstanceId::new();
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
    instance_id: &InstanceId,
    request_id: &str,
) -> Result<harnx_core::hooks::HookOutcome> {
    let provider =
        NatsHookProvider::discover_with_client(client.clone(), instance_id.clone()).await?;
    Ok(provider
        .dispatch_event(
            HookEvent::PreToolUse {
                tool_name: "exec".to_string(),
                tool_input: json!({"command": "true"}),
                tool_use_id: request_id.to_string(),
            },
            None,
            HookDispatchMeta {
                session_id: request_id.to_string(),
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
    instance_id: &InstanceId,
) -> Result<()> {
    let scopes = [
        ("global", "PreToolUse"),
        ("tool-time", "PostToolUse"),
        ("session-old", "SessionStart"),
        ("session-new", "Stop"),
    ];
    let mut active: Option<HookServerSupervisor> = None;
    for (scope, event) in scopes {
        if let Some(previous) = active.take() {
            let previous_name = previous
                .server_pids()
                .await
                .into_values()
                .next()
                .context("previous hook process")?;
            let previous_key = hook_registration_key(instance_id, &previous_name);
            drop(previous);
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
    let final_supervisor = active.context("final hook supervisor")?;
    let final_name = final_supervisor
        .server_pids()
        .await
        .into_values()
        .next()
        .context("final hook process")?;
    let final_key = hook_registration_key(instance_id, &final_name);
    drop(final_supervisor);
    wait_until_removed(store, &final_key).await
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
    let new_supervisor = active.context("new-agent hook supervisor")?;
    let new_name = new_supervisor
        .server_pids()
        .await
        .into_values()
        .next()
        .context("new-agent hook process")?;
    let new_key = hook_registration_key(&instance_id, &new_name);
    assert_ne!(old_key, new_key);
    assert!(store.get(&new_key).await?.is_some());

    drop(new_supervisor);
    wait_until_removed(&store, &new_key).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_hook_start_keeps_fail_closed_expectation() -> Result<()> {
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

    let supervisor = HookServerSupervisor::start_local_with_timeout(
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
    let key = hook_registration_key(&instance_id, "tool-bash-PreToolUse-0");
    assert!(store.get(&key).await?.is_some());

    let outcome = dispatch_pre_tool(&client, &instance_id, "failed-start").await?;
    assert!(matches!(outcome.control, HookResultControl::Block { .. }));

    drop(supervisor);
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
    let supervisor = HookServerSupervisor::start_local_with_timeout(
        start,
        &hook_config("PreToolUse"),
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
    assert!(registry.get(&key).await?.is_some());

    // SAFETY: pid belongs to child process owned by this test's supervisor.
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("SIGKILL hook server");
    }
    wait_until_removed(&registry, &key).await?;

    let outcome = dispatch_pre_tool(&client, &instance_id, "crashed-hook").await?;
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
    let mut hooks = hook_config("PreToolUse");
    hooks.entries[0].command = "printf '{}'".to_string();
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
