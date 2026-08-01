#[allow(dead_code)]
mod common;

use anyhow::{Context, Result};
use harnx_core::hooks::{HookConfig, HooksConfig};
use harnx_core::instance::InstanceId;
use harnx_hookset::HOOK_REGISTRY_BUCKET;
use harnx_hookset_server::hook_registration_key;
use harnx_runtime::nats_worker::{
    reconcile_hook_supervisor, HookServerStartConfig, HookServerSupervisor,
};
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
    ensure_hook_server_binary()?;
    let Some(server) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(TOKEN.to_string()),
    })
    .await?
    else {
        return Ok(());
    };
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(server.url())
        .await?;
    let instance_id = InstanceId::new();
    let start =
        HookServerStartConfig::new(client.clone(), instance_id.clone(), server.url(), TOKEN);

    let store = async_nats::jetstream::new(client.clone())
        .create_key_value(async_nats::jetstream::kv::Config {
            bucket: HOOK_REGISTRY_BUCKET.to_string(),
            ..Default::default()
        })
        .await?;

    run_scope_transitions(&start, &store, &instance_id).await
}

#[tokio::test(flavor = "multi_thread")]
async fn reconcile_hook_supervisor_replaces_old_agent_registration() -> Result<()> {
    harnx_core::require_nextest();
    ensure_hook_server_binary()?;
    let Some(server) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(TOKEN.to_string()),
    })
    .await?
    else {
        return Ok(());
    };
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(server.url())
        .await?;
    let instance_id = InstanceId::new();
    let start =
        HookServerStartConfig::new(client.clone(), instance_id.clone(), server.url(), TOKEN);
    let store = async_nats::jetstream::new(client)
        .create_key_value(async_nats::jetstream::kv::Config {
            bucket: HOOK_REGISTRY_BUCKET.to_string(),
            ..Default::default()
        })
        .await?;

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
