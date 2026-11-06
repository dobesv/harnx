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
        BashToolSelection, OperatorToolCommand, OperatorToolReply,
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

/// Build the operator Bash command for `harnx call bash` (#2630).
///
/// Called before configuration or worker startup, so a script that does not
/// parse fails here with a clear error instead of after claims were made.
/// Any `--tools`, `--tool` or `-t` presence selects explicit mode — even when
/// the selectors resolve to nothing — and disables inference. Without them
/// the literal command names come from bounded static analysis; they are
/// exact names, never selectors.
pub(crate) fn call_bash_command(
    cli: &Cli,
    args: &crate::cli::CallBashArgs,
) -> Result<OperatorToolCommand> {
    let explicit = !args.tools.is_empty() || !cli.tool.is_empty();
    // Brace-aware comma splitting: commas inside `{...}` are selector content
    // (e.g. fs_{read_file,write_file}), same as agent configuration.
    let mut selectors = Vec::new();
    for entry in cli.tool.iter().chain(&args.tools) {
        for selector in harnx_core::agent_config::split_tool_selectors(entry) {
            let selector = selector.trim();
            if !selector.is_empty() && !selectors.iter().any(|s| s == selector) {
                selectors.push(selector.to_string());
            }
        }
    }
    let selection = if explicit {
        BashToolSelection::Explicit { selectors }
    } else {
        let inference = harnx_script::tool_inference::infer_script_tools(&args.script)
            .map_err(anyhow::Error::msg)?;
        BashToolSelection::Inferred {
            names: inference.names,
        }
    };
    Ok(OperatorToolCommand::Bash {
        script: args.script.clone(),
        selection,
    })
}

fn reservation_view(command: &OperatorToolCommand) -> ToolReservationView {
    let selector = match command {
        OperatorToolCommand::Info { name } | OperatorToolCommand::Call { name, .. } => name.clone(),
        OperatorToolCommand::List { pattern } => pattern.clone().unwrap_or_else(|| "*".into()),
        // Startup claims only; the worker still owns admission (#2630 task 4).
        // Inferred names are exact literals, never expanded selectors.
        OperatorToolCommand::Bash { selection, .. } => {
            let use_tools = match selection {
                BashToolSelection::Inferred { names } => names.clone(),
                BashToolSelection::Explicit { selectors } => selectors.clone(),
                // Session mode is TUI-only and never reaches this CLI path.
                BashToolSelection::Session => Vec::new(),
            };
            return ToolReservationView {
                package: None,
                use_tools,
            };
        }
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
                    bash_timeout_secs: cli.timeout_secs.filter(|seconds| *seconds > 0),
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
                bash_timeout_secs: cli.timeout_secs.filter(|seconds| *seconds > 0),
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
    use clap::Parser;
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
    fn bash_reservation_claims_follow_selection_without_inference() {
        // Startup claims only; the worker still owns admission (#2630 task 4).
        let view = reservation_view(&OperatorToolCommand::Bash {
            script: "fs_read a".into(),
            selection: BashToolSelection::Inferred {
                names: vec!["fs_read".into()],
            },
        });
        assert_eq!(view.use_tools, ["fs_read"]);
        let view = reservation_view(&OperatorToolCommand::Bash {
            script: "fs_read a".into(),
            selection: BashToolSelection::Explicit {
                selectors: vec!["fs_*".into()],
            },
        });
        assert_eq!(view.use_tools, ["fs_*"]);
        // Explicit-but-empty claims nothing: inference stays disabled and
        // there is no wildcard fallback.
        let view = reservation_view(&OperatorToolCommand::Bash {
            script: "fs_read a".into(),
            selection: BashToolSelection::Explicit { selectors: vec![] },
        });
        assert!(view.use_tools.is_empty());
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

    /// Parse a `call bash` argv and return (global --tool entries, subcommand
    /// args), avoiding Cli's lack of Clone.
    fn bash_cli(
        tools: &[&str],
        global_tool: &[&str],
        script: &str,
    ) -> (Vec<String>, crate::cli::CallBashArgs) {
        let mut argv: Vec<&str> = vec!["harnx"];
        for value in global_tool {
            argv.extend(["--tool", value]);
        }
        argv.extend(["call", "bash", script]);
        for entry in tools {
            argv.extend(["--tools", entry]);
        }
        argv.push("--json");
        let cli = Cli::try_parse_from(argv).expect("call bash grammar");
        let legacy = cli.tool.clone();
        let Some(crate::cli::Commands::Call(call)) = cli.command else {
            panic!("expected call command");
        };
        let crate::cli::CallSubcommands::Bash(args) = call.command else {
            panic!("expected call bash");
        };
        (legacy, args)
    }

    fn bash_command(
        (legacy, args): &(Vec<String>, crate::cli::CallBashArgs),
    ) -> Result<OperatorToolCommand> {
        let cli = Cli::try_parse_from(
            std::iter::once("harnx".to_string()).chain(
                legacy
                    .iter()
                    .flat_map(|value| ["--tool".to_string(), value.clone()]),
            ),
        )
        .expect("global tool flags parse");
        call_bash_command(&cli, args)
    }

    #[test]
    fn call_bash_infers_literal_names_without_explicit_selectors() {
        let parsed = bash_cli(&[], &[], "echo hi");
        let command = bash_command(&parsed).unwrap();
        assert_eq!(
            command,
            OperatorToolCommand::Bash {
                script: "echo hi".into(),
                selection: BashToolSelection::Inferred {
                    names: vec!["echo".into()]
                }
            }
        );
    }

    #[test]
    fn call_bash_explicit_tools_are_brace_aware_deduplicated_and_trimmed() {
        // Repeated --tools entries, comma lists and nested braces all merge;
        // commas inside fs_{read,write} are selector content, not separators.
        let parsed = bash_cli(&["fs_read, fs_{read,write}", "toolset_a"], &[], "echo hi");
        let command = bash_command(&parsed).unwrap();
        assert_eq!(
            command,
            OperatorToolCommand::Bash {
                script: "echo hi".into(),
                selection: BashToolSelection::Explicit {
                    selectors: vec![
                        "fs_read".into(),
                        "fs_{read,write}".into(),
                        "toolset_a".into()
                    ]
                }
            }
        );
    }

    #[test]
    fn call_bash_legacy_tool_flag_is_additive_explicit_selection() {
        // The existing hidden global --tool/-t stays compatible and additive
        // with --tools; either presence disables inference.
        let parsed = bash_cli(&["from_tools"], &["legacy_tool"], "echo hi");
        let command = bash_command(&parsed).unwrap();
        assert_eq!(
            command,
            OperatorToolCommand::Bash {
                script: "echo hi".into(),
                selection: BashToolSelection::Explicit {
                    selectors: vec!["legacy_tool".into(), "from_tools".into()]
                }
            }
        );
    }

    #[test]
    fn call_bash_explicit_empty_disables_inference() {
        // `--tools ''` resolves to no selectors but is still explicit: a pure
        // shell run that must not fall back to inference.
        let parsed = bash_cli(&[""], &[], "echo hi");
        let command = bash_command(&parsed).unwrap();
        assert_eq!(
            command,
            OperatorToolCommand::Bash {
                script: "echo hi".into(),
                selection: BashToolSelection::Explicit { selectors: vec![] }
            }
        );
    }

    #[test]
    fn call_bash_parse_failure_is_a_clear_error_before_startup() {
        // `if then` does not parse: a clear error, not an empty selection.
        let parsed = bash_cli(&[], &[], "if then");
        let error = bash_command(&parsed).unwrap_err().to_string();
        assert!(error.contains("cannot analyze bash script"), "{error}");
        // Explicit mode never analyzes the script for selection, so it must
        // not fail here; the worker still refuses to run a broken script.
        let parsed = bash_cli(&["fs_read"], &[], "if then");
        assert!(bash_command(&parsed).is_ok());
    }
}
