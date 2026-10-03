use crate::cli::Cli;
use anyhow::{ensure, Context, Result};
use harnx_core::abort::{create_abort_signal, wait_abort_signal, AbortSignal};
use harnx_runtime::{
    config::WorkingMode,
    local_orchestrator::activation_route_for_cluster,
    nats_worker::tool_reservation::ToolReservationView,
    operator_tools::{
        cli::{
            run_named_tool_command, run_reserved_tool_command, NamedToolTarget,
            ToolCommandExecution,
        },
        OperatorToolCommand, OperatorToolReply,
    },
    tool_reservation_client::ToolReservationHandle,
};
use std::{future::Future, sync::Arc, time::Duration};

fn validate(command: &OperatorToolCommand) -> Result<()> {
    if let OperatorToolCommand::Call { args_json, .. } = command {
        let args: serde_json::Value =
            serde_json::from_str(args_json).context("Invalid tool argument JSON")?;
        ensure!(args.is_object(), "Tool arguments must be a JSON object");
    }
    Ok(())
}

fn reservation_view(command: &OperatorToolCommand) -> ToolReservationView {
    let selector = match command {
        OperatorToolCommand::Info { name } | OperatorToolCommand::Call { name, .. } => name.clone(),
        OperatorToolCommand::List { pattern } => pattern.clone().unwrap_or_else(|| "*".into()),
    };
    ToolReservationView {
        package: None,
        use_tools: vec![selector],
    }
}

/// Timeout/cancellation does not replay calls. Retain full output if cancellation
/// completes with a result, but never turn an unknown outcome into exit status 0.
async fn bounded_operation(
    operation: impl Future<Output = Result<OperatorToolReply>>,
    timeout: Duration,
    abort: &AbortSignal,
) -> Result<OperatorToolReply> {
    tokio::pin!(operation);
    let reason = tokio::select! {
        biased;
        result = &mut operation => return result,
        _ = wait_abort_signal(abort) => "Operator tool command cancelled; outcome may be unknown",
        _ = tokio::time::sleep(timeout) => "Operator tool command timed out; outcome may be unknown",
    };
    abort.set_ctrlc();
    let mut reply = tokio::time::timeout(Duration::from_secs(10), &mut operation)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or(OperatorToolReply {
            output: String::new(),
            error: None,
        });
    reply.error = Some(reason.into());
    Ok(reply)
}

async fn execute(cli: &Cli, command: OperatorToolCommand, json: bool) -> Result<OperatorToolReply> {
    // Reject JSON before loading configuration or starting any worker/tool process.
    validate(&command)?;
    let abort = create_abort_signal();
    let signal_abort = abort.clone();
    let signal_task = scopeguard::guard(
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                signal_abort.set_ctrlc();
            }
        }),
        |task| task.abort(),
    );
    let CallerSelection {
        config,
        named,
        cluster,
    } = resolve_caller(cli).await?;
    let supervisor = Arc::new(tokio::sync::Mutex::new(None));
    let route = tokio::time::timeout(
        Duration::from_secs(120),
        activation_route_for_cluster(&cluster, &supervisor, abort.clone()),
    )
    .await
    .context("Tool worker startup timed out")??;
    let timeout = Duration::from_secs(
        cli.timeout_secs
            .filter(|seconds| *seconds > 0)
            .unwrap_or(600),
    );
    let result = if let Some((agent, cluster)) = named {
        bounded_operation(
            run_named_tool_command(
                &config,
                NamedToolTarget {
                    agent: &agent,
                    cluster: &cluster,
                    route,
                },
                ToolCommandExecution {
                    command: &command,
                    json,
                    abort: &abort,
                },
            ),
            timeout,
            &abort,
        )
        .await
    } else {
        execute_reserved(
            config,
            route,
            ToolCommandExecution {
                command: &command,
                json,
                abort: &abort,
            },
            timeout,
        )
        .await
    };
    drop(signal_task);
    // Reservation release precedes local worker teardown.
    supervisor.lock().await.take();
    result
}

struct CallerSelection {
    config: harnx_runtime::config::Config,
    named: Option<(String, String)>,
    cluster: String,
}

async fn resolve_caller(cli: &Cli) -> Result<CallerSelection> {
    let mut config = tokio::time::timeout(
        Duration::from_secs(60),
        crate::init_frontend_config(WorkingMode::Cmd, true),
    )
    .await
    .context("Tool command configuration timed out")??;
    // This command chooses its own caller; neither active nor default agents apply.
    config.agent = None;
    config.remote_agent = None;
    config.session = None;
    let named = cli
        .agent
        .as_deref()
        .map(|agent| config.resolve_session_agent(agent))
        .transpose()?;
    let cluster = named
        .as_ref()
        .map(|(_, cluster)| cluster.as_str())
        .unwrap_or_else(|| config.default_cluster_key())
        .to_owned();
    // Make the captured route authoritative for discovery and reservation renewal.
    if cluster != harnx_runtime::config::LOCAL_CLUSTER_KEY {
        config.nats_server(&cluster)?;
        config.nats_routing = harnx_runtime::config::NatsRouting::Cluster(cluster.clone());
    }
    Ok(CallerSelection {
        config,
        named,
        cluster,
    })
}

async fn execute_reserved(
    config: harnx_runtime::config::Config,
    route: harnx_runtime::SessionActivationRoute,
    execution: ToolCommandExecution<'_>,
    timeout: Duration,
) -> Result<OperatorToolReply> {
    let view = reservation_view(execution.command);
    let mut reservation = tokio::time::timeout(
        Duration::from_secs(120),
        ToolReservationHandle::open(config, route, view.clone()),
    )
    .await
    .context("Tool reservation startup timed out")??;
    let result = bounded_operation(
        run_reserved_tool_command(&reservation, &view, execution),
        timeout,
        execution.abort,
    )
    .await;
    let cleanup = reservation.close().await;
    match (result, cleanup) {
        (Ok(mut reply), Err(error)) => {
            reply.error = Some(format!("Tool reservation cleanup failed: {error:#}"));
            Ok(reply)
        }
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Tool reservation cleanup also failed: {cleanup:#}")))
        }
        (result, Ok(())) => result,
    }
}

pub(crate) async fn run(cli: &Cli, command: OperatorToolCommand, json: bool) -> Result<u8> {
    let reply = execute(cli, command, json)
        .await
        .unwrap_or_else(|error| OperatorToolReply {
            output: String::new(),
            error: Some(format!("{error:#}")),
        });
    let failed = reply.error.is_some();
    if !reply.output.is_empty() {
        println!("{}", reply.output);
    } else if json {
        // Metadata/discovery/protocol failures have no provider result. Keep stdout
        // parseable without replacing actual tool error or partial result payloads.
        println!(
            "{}",
            serde_json::json!({"isError": true, "error": reply.error})
        );
    }
    if let Some(error) = reply.error {
        eprintln!("error: {error}");
    }
    Ok(u8::from(failed))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn virtual_view_never_uses_a_default_agent() {
        assert_eq!(
            reservation_view(&OperatorToolCommand::List { pattern: None }).use_tools,
            ["*"]
        );
        assert_eq!(
            reservation_view(&OperatorToolCommand::List {
                pattern: Some("fs_*".into())
            })
            .use_tools,
            ["fs_*"]
        );
        let view = reservation_view(&OperatorToolCommand::Info {
            name: "fs_read".into(),
        });
        assert_eq!(view.use_tools, ["fs_read"]);
        assert_eq!(view.package, None);
    }
    #[test]
    fn arguments_are_object_json_before_runtime_setup() {
        for args in ["null", "[]", "1", "true", "{bad"] {
            assert!(validate(&OperatorToolCommand::Call {
                name: "fs_read".into(),
                args_json: args.into()
            })
            .is_err());
        }
        validate(&OperatorToolCommand::Call {
            name: "fs_read".into(),
            args_json: r#"{"path":"a b", "quote":"\""}"#.into(),
        })
        .unwrap();
    }
    #[tokio::test]
    async fn timeout_retains_partial_output_and_cancels() {
        let abort = create_abort_signal();
        let operation = async {
            wait_abort_signal(&abort).await;
            Ok(OperatorToolReply {
                output: r#"{"partial":true,"data":[1]}"#.into(),
                error: None,
            })
        };
        let reply = bounded_operation(operation, Duration::from_millis(1), &abort)
            .await
            .unwrap();
        assert!(abort.aborted());
        assert!(reply.error.unwrap().contains("timed out"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&reply.output).unwrap()["data"],
            serde_json::json!([1])
        );
    }
}
