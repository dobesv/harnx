//! Cold-start coverage: only the SessionStart hook requests the helper server.

use super::*;
use anyhow::ensure;
use futures_util::StreamExt;
use harnx_runtime::config::{GlobalConfig, ToolServerConfig};
use harnx_toolset::Registration;
use std::path::{Path, PathBuf};
use tokio_util::task::AbortOnDropHandle;

const HELPER_OUTPUT: &str = "context read from a cold-started helper server";
const NO_HELPER_CONTEXT: &str = "hook has no helper tools";

fn fs_tools_binary() -> Result<PathBuf> {
    let binary = harnx_core::bin_dir_of_exe(&std::env::current_exe()?)
        .join(format!("harnx-fs-tools{}", std::env::consts::EXE_SUFFIX));
    ensure!(
        binary.is_file(),
        "build workspace first: {} is missing",
        binary.display()
    );
    Ok(binary)
}

fn cold_start_config(url: &str, dir: &Path, with_tools: bool) -> Result<GlobalConfig> {
    let input = serde_json::to_string(&dir.join("context.txt"))?;
    let forbidden = serde_json::to_string(&dir.join("forbidden.txt"))?;
    let hooks = HooksConfig {
        session_start: EventHooksList {
            entries: vec![EventHookEntry::Full(EventHookDefinition {
                name: "cold-start-helper".to_string(),
                source: MiddlewareSourceConfig::Bash {
                    bash: format!(
                        r#"set -e
# The same server advertises write, but the hook only allows read.
if fs_write --path {forbidden} --content 'must not be written'; then
    exit 1
fi
if command -v fs_read >/dev/null; then
    context="$(fs_read --path {input} -o text)"
    add_context "$context"
else
    add_context '{NO_HELPER_CONTEXT}'
fi"#
                    ),
                },
                tools: if with_tools {
                    vec!["fs_read".to_string()]
                } else {
                    vec![]
                },
                fetch: vec![],
                timeout: CI_SAFE_TIMEOUT,
                on_error: EventHookOnError::Deny,
            })],
        },
        ..Default::default()
    };
    let config = daemon_config_with_hooks(url, hooks);
    {
        let mut config = config.write();
        // No model, agent, or middleware selector can start the helper.
        config.use_tools = Some(vec![]);
        config.tool_servers = vec![ToolServerConfig {
            name: "fs".to_string(),
            command: fs_tools_binary()?.to_string_lossy().into_owned(),
            // Permit writes at the provider so an allowlist bypass would create
            // forbidden.txt, rather than being masked by filesystem policy.
            args: vec![
                "--name".to_string(),
                "fs".to_string(),
                "--allow-write".to_string(),
                dir.to_string_lossy().into_owned(),
            ],
            env: Default::default(),
            enabled: true,
            description: None,
            package: None,
            hooks: None,
            middleware: Default::default(),
        }];
        assert!(config.agent.is_none());
        assert!(config.resolved_middleware()?.entries.is_empty());
        assert!(config.nats_tool_declarations.read().is_empty());
    }
    Ok(config)
}

/// Observe real registrations, including transient ones, without publishing a
/// declaration or starting a server. A same-connection fence bounds each read.
async fn helper_registrations(
    client: &async_nats::Client,
    observed: &mut async_nats::Subscriber,
) -> Result<Vec<Registration>> {
    let fence = format!("test.session_start.fence.{}", uuid::Uuid::new_v4());
    client.publish(fence.clone(), "".into()).await?;
    client.flush().await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        let mut registrations = Vec::new();
        while let Some(message) = observed.next().await {
            if message.subject.as_str() == fence {
                return Ok(registrations);
            }
            if message.subject.as_str().starts_with(&format!(
                "$KV.{}.",
                harnx_toolset_server::TOOL_REGISTRY_BUCKET
            )) && !message.payload.is_empty()
            {
                let registration: Registration = serde_json::from_slice(&message.payload)?;
                if registration.server == "fs" {
                    registrations.push(registration);
                }
            }
        }
        anyhow::bail!("registration observer closed before fence")
    })
    .await
    .context("registration observer did not reach fence")?
}

async fn run_cold_start(with_tools: bool) -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("context.txt"), HELPER_OUTPUT)?;
    let _env = [
        EnvGuard::set("HARNX_NATS_URL", server.url()),
        EnvGuard::set("HARNX_NATS_TOKEN", "session-start-test"),
        EnvGuard::set(
            "HARNX_CONFIG_DIR",
            dir.path().to_str().context("config path")?,
        ),
    ];
    let config = cold_start_config(server.url(), dir.path(), with_tools)?;
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client.clone());
    let mut observed = client.subscribe(">".to_string()).await?;
    // flush only writes the buffer; a round trip ensures observation is active
    // before the worker can publish anything.
    js.query_account().await?;
    let requests: RecordedRequests = Arc::new(AsyncMutex::new(Vec::new()));
    let daemon = AbortOnDropHandle::new(
        spawn_worker_daemon_with_call_fn(
            config,
            "session-start-cold-worker",
            recording_call_fn(Arc::clone(&requests), "cold start done", false),
        )
        .await?,
    );
    assert!(
        helper_registrations(&client, &mut observed)
            .await?
            .is_empty(),
        "configured helper must not start before session activation"
    );

    let session_id = "session-start-cold";
    let session = worker_session(server.url(), &js, session_id).await?;
    let metrics_before = harnx_runtime::nats_metrics::snapshot();
    session
        .enqueue_text("worker prompt")
        .await?
        .into_activation_result()?;
    wait_for_worker_daemon_idle(&js, session_id, metrics_before.lease_acquisitions).await?;
    wait_for_requests(&requests, 1).await?;

    let registrations = helper_registrations(&client, &mut observed).await?;
    assert_eq!(
        !registrations.is_empty(),
        with_tools,
        "only the hook's tools entry should cause helper startup: {registrations:?}"
    );
    if with_tools {
        let tools = &registrations[0].tools;
        assert!(tools.iter().any(|tool| tool.name == "read"));
        assert!(tools.iter().any(|tool| tool.name == "write"));
    }
    assert!(
        !dir.path().join("forbidden.txt").exists(),
        "non-allowlisted write must never reach the helper"
    );
    let requests = requests.lock().await.clone();
    assert_eq!(
        requests.len(),
        1,
        "one completed model request: {requests:?}"
    );
    let expected = if with_tools {
        HELPER_OUTPUT
    } else {
        NO_HELPER_CONTEXT
    };
    assert_eq!(
        requests[0]
            .iter()
            .filter(|text| text.contains(expected))
            .count(),
        1,
        "SessionStart add_context must reach built model messages once: {requests:?}"
    );
    let log = NatsSessionLog::new_with_replicas(js, storage_key(session_id), 1);
    let entries = log.load_events_async().await?;
    assert!(assistant_texts(&entries)
        .iter()
        .any(|text| text == "cold start done"));
    assert!(!entries.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::Error { .. }
            | SessionLogEntry::ToolCalls { .. }
            | SessionLogEntry::ToolResults { .. }
    )));
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_start_hook_tools_cold_start_reaches_model() -> Result<()> {
    run_cold_start(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_start_without_hook_tools_does_not_start_helper() -> Result<()> {
    run_cold_start(false).await
}
