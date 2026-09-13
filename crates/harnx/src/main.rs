mod agent_event_sink;
mod cli;
mod cli_event_sink;
mod oneshot_nats;

/// Heap-usage guard installed as the process allocator: aborts with a backtrace
/// if live heap exceeds `HARNX_HEAP_LIMIT_MB`. Disarmed (plain passthrough to
/// the system allocator) when that env var is unset. Diagnostic for the #842
/// runaway-allocation OOM.
#[global_allocator]
static GLOBAL_ALLOC: harnx_core::alloc_guard::HeapGuard = harnx_core::alloc_guard::HeapGuard;

#[cfg(test)]
pub mod test_utils;

pub use harnx_core::safety as mcp_safety;
pub use harnx_runtime::{client, commands, config, tool};
pub use harnx_tui as tui;

use crate::cli::{
    Cli, Commands, DeleteSessionArgs, DeleteSubcommands, DumpSubcommands, InfoSubcommands,
    ListSubcommands,
};
use crate::client::{list_models, ModelType};
use crate::config::{
    list_agents, list_assistant_agents, load_env_file, macro_execute, render_agent_dump, Config,
    GlobalConfig, Input, WorkingMode,
};
use crate::tui::{TranscriptItem, Tui};
use harnx_core::agent_config::collect_agent_variables;
use harnx_core::event::AgentSource;
use harnx_render::{render_error, MarkdownRender};
use harnx_runtime::config::SessionMeta;
use harnx_runtime::utils::*;

use anyhow::{bail, Context, Result};
use clap::Parser;
use parking_lot::RwLock;
use std::{sync::Arc, time::Duration};

use harnx_runtime::remote_session_cleanup::{run_remote_cleanup, RemoteCleanupStats};

fn invocation_limit_reached(error: &anyhow::Error) -> bool {
    error.is::<oneshot_nats::InvocationLimitReached>()
}

/// Routing decision for `list sessions` handler.
/// Extracted as a pure function for testability.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ListSessionsTarget {
    /// List sessions from the shared local NATS cluster.
    Local,
    /// List sessions from a remote NATS cluster.
    Remote { cluster: String },
}

/// Pure routing decision for `list sessions`.
/// Given the remote agent context, returns whether to list local or remote sessions.
///
/// This function encapsulates the branch selection logic so it can be unit-tested
/// without requiring a live NATS cluster or mocking async I/O.
fn resolve_list_sessions_target(remote_agent: Option<&(String, String)>) -> ListSessionsTarget {
    match remote_agent {
        Some((_, cluster)) => ListSessionsTarget::Remote {
            cluster: cluster.clone(),
        },
        None => ListSessionsTarget::Local,
    }
}

/// Format session metadata as one ID per line.
/// This helper is extracted for testability without touching stdout.
fn format_sessions_for_output(sessions: &[SessionMeta]) -> String {
    let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    ids.join("\n")
}

/// Outcome of a remote list-sessions operation.
/// This pure helper enables unit-testing the error-propagation path without
/// mocking the async NATS client or relying on assertion side-effects.
#[derive(Debug, PartialEq, Eq)]
enum ListSessionsOutcome {
    /// Sessions fetched successfully; output to be printed to stdout.
    Print(String),
    /// Remote fetch failed; error message for stderr and non-zero exit.
    Error(String),
}

/// Map a remote list-sessions result to an outcome for the CLI.
/// Ok(sessions) → Print(formatted output)
/// Err(e) → Error(error message)
///
/// This helper exists to make error-propagation genuinely testable.
/// The handler calls this and then performs the actual println/eprintln/return-Err.
fn remote_list_outcome(result: Result<Vec<SessionMeta>, anyhow::Error>) -> ListSessionsOutcome {
    match result {
        Ok(sessions) => ListSessionsOutcome::Print(format_sessions_for_output(&sessions)),
        Err(e) => ListSessionsOutcome::Error(format!("{e:#}")),
    }
}

#[tokio::main]
async fn main() -> Result<std::process::ExitCode> {
    load_env_file()?;
    let cli = Cli::parse();
    setup_logger(LogSink::File)?;
    let telemetry = harnx_telemetry::init_telemetry("harnx")?;
    harnx_core::alloc_guard::init_from_env();

    let result = run_main(cli).await;
    telemetry.shutdown().await;
    if let Some(error) = result? {
        if invocation_limit_reached(&error) {
            return Ok(std::process::ExitCode::from(
                oneshot_nats::INVOCATION_LIMIT_EXIT_CODE as u8,
            ));
        }
        render_error(error);
        return Ok(std::process::ExitCode::FAILURE);
    }
    // Returning drops the Tokio runtime and its broker supervision tasks.
    // process::exit bypasses that cleanup, leaving a broker bound on platforms
    // without Linux's parent-death signal and preventing immediate restart.
    Ok(std::process::ExitCode::SUCCESS)
}

async fn run_main(cli: Cli) -> Result<Option<anyhow::Error>> {
    match &cli.command {
        Some(
            command @ (Commands::Info(_)
            | Commands::Dump(_)
            | Commands::Delete(_)
            | Commands::List(_)),
        ) => {
            run_command(command).await?;
            return Ok(None);
        }
        Some(Commands::Prompt(_)) | None => {}
    }

    let text = cli.text()?;
    let working_mode = match (&cli.command, &text, cli.file.is_empty()) {
        (Some(Commands::Prompt(_)), _, _) | (_, Some(_), _) | (_, _, false) => WorkingMode::Cmd,
        _ => WorkingMode::Tui,
    };
    let info_flag = legacy_info_flag(&cli);
    let config = Arc::new(RwLock::new(Config::init(working_mode, info_flag).await?));
    Ok(run(config, cli, text).await.err())
}

async fn run_command(command: &Commands) -> Result<()> {
    match command {
        Commands::Prompt(_) => bail!("prompt commands use the one-shot execution path"),
        Commands::Info(info_args) => run_info_command(info_args).await,
        Commands::Dump(dump_args) => run_dump_command(dump_args).await,
        Commands::Delete(delete_args) => run_delete_command(delete_args).await,
        Commands::List(list_args) => run_list_command(list_args).await,
    }
}

async fn run_info_command(info_args: &crate::cli::InfoArgs) -> Result<()> {
    match &info_args.command {
        InfoSubcommands::Agent { name } => {
            let config = Config::init(WorkingMode::Cmd, true).await?;
            let out = render_agent_dump(&config, name)?;
            println!("{out}");
            Ok(())
        }
        InfoSubcommands::Session {
            agent_name,
            session_id,
            format,
        } => run_info_session(agent_name, session_id, format).await,
    }
}

async fn run_info_session(
    agent_name: &str,
    session_id: &str,
    format: &harnx_runtime::config::SessionFormat,
) -> Result<()> {
    use harnx_runtime::config::SessionFormat;
    let config = Config::init(WorkingMode::Cmd, true).await?;
    match format {
        SessionFormat::Text => {
            let session = harnx_runtime::config::load_session_for_render(
                &config, None, session_id, agent_name,
            )
            .await?;
            let out = harnx_runtime::config::session::render(&session)?;
            println!("{out}");
        }
        SessionFormat::Yaml | SessionFormat::Json => {
            let metadata = fetch_session_metadata(&config, session_id, agent_name).await?;
            let out = match format {
                SessionFormat::Yaml => harnx_runtime::config::render_metadata_yaml(&metadata)?,
                SessionFormat::Json => harnx_runtime::config::render_metadata_json(&metadata)?,
                SessionFormat::Text => unreachable!(),
            };
            println!("{out}");
        }
    }
    Ok(())
}

async fn fetch_session_metadata(
    config: &Config,
    session_id: &str,
    agent_name: &str,
) -> Result<harnx_runtime::nats_session_metadata::SessionMetadata> {
    let jetstream = config
        .nats_jetstream(harnx_runtime::config::LOCAL_CLUSTER_KEY)
        .await?;
    let store =
        harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1).await?;
    let record = store
        .get_for_agent(session_id, agent_name)
        .await?
        .with_context(|| format!("Session '{session_id}' for agent '{agent_name}' not found"))?;
    Ok(record.metadata)
}

async fn run_dump_command(dump_args: &crate::cli::DumpArgs) -> Result<()> {
    match &dump_args.command {
        DumpSubcommands::Session {
            agent_name,
            session_id,
            format,
            follow,
        } => {
            if *follow {
                return run_dump_session_follow(session_id, agent_name, *format).await;
            }
            run_dump_session_once(session_id, agent_name, format).await
        }
    }
}

async fn run_dump_session_once(
    session_id: &str,
    agent_name: &str,
    format: &harnx_runtime::config::SessionFormat,
) -> Result<()> {
    use harnx_runtime::config::SessionFormat;
    let config = Config::init(WorkingMode::Cmd, true).await?;

    // Validate session exists for this agent
    let jetstream = config
        .nats_jetstream(harnx_runtime::config::LOCAL_CLUSTER_KEY)
        .await?;
    let store =
        harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1).await?;
    store
        .get_for_agent(session_id, agent_name)
        .await?
        .with_context(|| format!("Session '{session_id}' for agent '{agent_name}' not found"))?;

    // Load raw entries and reconstruct
    let log =
        harnx_runtime::nats_session_log::NatsSessionLog::new(jetstream, session_id.to_string());
    let raw = log.load_events_async().await?;
    let entries = harnx_core::session_reconstruct::apply_log_mutations_nats(&raw)?;

    match format {
        SessionFormat::Text => {
            // Build CliAgentEventSink and replay entries
            use crate::cli_event_sink::CliAgentEventSink;
            use harnx_core::abort::create_abort_signal;
            use harnx_render::RenderOptions;
            let render_options = RenderOptions::default();
            let abort_signal = create_abort_signal();
            let sink = Arc::new(CliAgentEventSink::new(false, render_options, abort_signal));
            harnx_runtime::replay_entries_to_sink(&entries, sink);
        }
        SessionFormat::Yaml => {
            let out = harnx_runtime::config::dump_entries_yaml(entries.iter().map(|(_, e)| e))?;
            print!("{out}");
        }
        SessionFormat::Json => {
            let out = harnx_runtime::config::dump_entries_jsonl(entries.iter().map(|(_, e)| e))?;
            print!("{out}");
        }
    }
    Ok(())
}

async fn run_dump_session_follow(
    session_id: &str,
    agent_name: &str,
    format: harnx_runtime::config::SessionFormat,
) -> Result<()> {
    use std::io::Write;

    let config = Config::init(WorkingMode::Cmd, true).await?;
    let cluster = harnx_runtime::config::LOCAL_CLUSTER_KEY;

    // Validate session exists for this agent and get jetstream context
    let jetstream = config.nats_jetstream(cluster).await?;
    let store =
        harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1).await?;
    store
        .get_for_agent(session_id, agent_name)
        .await?
        .with_context(|| format!("Session '{session_id}' for agent '{agent_name}' not found"))?;

    // Attach to session event stream (uses same jetstream context)
    let client = config.nats_client(cluster).await?;
    let mut stream =
        harnx_runtime::nats_event_sink::SessionEventStream::attach(jetstream, client, session_id)
            .await?;

    // Replay initial history
    replay_dump_entries(stream.history(), &format).await?;
    std::io::stdout().flush()?;

    // Follow loop: durable-only, with periodic poll timeout for lossy advisories
    loop {
        tokio::select! {
            // Advisory wake-up (lossy)
            _ = stream.next() => {}
            // Periodic poll timeout — REQUIRED for entries with no advisory (e.g. TurnEnd)
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(1000)) => {}
            // Clean exit on Ctrl-C
            _ = tokio::signal::ctrl_c() => {
                return Ok(());
            }
        }

        // On wake, check for new durable entries
        let old_len = stream.history().len();
        if stream.refresh_history().await? {
            let new_entries = &stream.history()[old_len..];
            if !new_entries.is_empty() {
                replay_dump_entries(new_entries, &format).await?;
                std::io::stdout().flush()?;
            }
        }
    }
}

async fn replay_dump_entries(
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
    format: &harnx_runtime::config::SessionFormat,
) -> Result<()> {
    use harnx_runtime::config::SessionFormat;

    match format {
        SessionFormat::Text => {
            use crate::cli_event_sink::CliAgentEventSink;
            use harnx_core::abort::create_abort_signal;
            use harnx_render::RenderOptions;
            let render_options = RenderOptions::default();
            let abort_signal = create_abort_signal();
            let sink = Arc::new(CliAgentEventSink::new(false, render_options, abort_signal));
            harnx_runtime::replay_entries_to_sink(entries, sink);
        }
        SessionFormat::Yaml => {
            for (_, entry) in entries {
                let doc = harnx_runtime::config::yaml_doc(entry)?;
                print!("{doc}");
            }
        }
        SessionFormat::Json => {
            for (_, entry) in entries {
                let line = harnx_runtime::config::jsonl_line(entry)?;
                print!("{line}");
            }
        }
    }
    Ok(())
}

async fn run_delete_command(delete_args: &crate::cli::DeleteArgs) -> Result<()> {
    match &delete_args.command {
        DeleteSubcommands::Session(args) => run_session_delete_command(args).await,
    }
}

async fn run_list_command(list_args: &crate::cli::ListArgs) -> Result<()> {
    match &list_args.command {
        ListSubcommands::Sessions => run_list_sessions().await,
    }
}

async fn run_list_sessions() -> Result<()> {
    let config = Config::init(WorkingMode::Cmd, true).await?;

    let target = resolve_list_sessions_target(config.remote_agent.as_ref());
    match target {
        ListSessionsTarget::Remote { cluster } => {
            let result = config.list_remote_sessions_with_meta(&cluster).await;
            match remote_list_outcome(result) {
                ListSessionsOutcome::Print(output) => {
                    println!("{output}");
                }
                ListSessionsOutcome::Error(msg) => {
                    eprintln!("error: could not list sessions for cluster '{cluster}': {msg}");
                    return Err(anyhow::anyhow!("{msg}"));
                }
            }
        }
        ListSessionsTarget::Local => {
            let sessions = config
                .list_remote_sessions_with_meta(harnx_runtime::config::LOCAL_CLUSTER_KEY)
                .await?;
            println!("{}", format_sessions_for_output(&sessions));
        }
    }
    Ok(())
}

async fn run_session_delete_command(delete_args: &DeleteSessionArgs) -> Result<()> {
    let config = Config::init(WorkingMode::Cmd, true).await?;
    let result = harnx_runtime::nats_admin::delete_remote_session(
        &config,
        &delete_args.cluster,
        &delete_args.session_id,
    )
    .await?;

    if result.removed_anything() {
        println!(
            "Deleted remote session '{}' on cluster '{}' (stream_deleted={}, lease_deleted={}, attachments_deleted={})",
            delete_args.session_id,
            delete_args.cluster,
            result.stream_deleted,
            result.lease_deleted,
            result.attachments_deleted
        );
    } else {
        println!(
            "Remote session '{}' on cluster '{}' not found; nothing to delete.",
            delete_args.session_id, delete_args.cluster
        );
    }

    Ok(())
}

fn legacy_info_flag(cli: &Cli) -> bool {
    cli.info
        || cli.sync_models
        || cli.list_models
        || cli.list_agents
        || cli.list_assistant_agents
        || cli.list_rags
        || cli.list_macros
}

fn command_only_needs_supplied_session(cli: &Cli) -> bool {
    cli.info
}

async fn apply_session_arg(config: &GlobalConfig, cli: &Cli) -> Result<()> {
    let Some(session) = &cli.session else {
        return Ok(());
    };
    if session.is_none() && command_only_needs_supplied_session(cli) {
        return Ok(());
    }
    let session = match session {
        Some(s) => s.clone(),
        None => Config::reserve_new_session_id(config).await?,
    };
    config.write().use_session(Some(&session))
}

async fn activate_cli_agent(
    config: &GlobalConfig,
    cli: &Cli,
    agent: &str,
    abort_signal: &AbortSignal,
) -> Result<()> {
    config.write().agent_variables = collect_agent_variables(&cli.agent_variable)?;

    // A bare --session must reserve its canonical metadata only after the
    // requested agent is active. Reserving first would snapshot the
    // pre-activation (usually temporary inline) configuration and make the
    // immutable identity disagree with the first turn.
    let result = async {
        if command_only_needs_supplied_session(cli) {
            let session = cli.session.as_ref().and_then(Option::as_deref);
            return Config::use_agent(config, agent, session, abort_signal.clone()).await;
        }
        match cli.session.as_ref() {
            Some(None) => {
                Config::use_agent(config, agent, None, abort_signal.clone()).await?;
                let session_id = Config::reserve_new_session_id(config).await?;
                config.write().use_session(Some(&session_id))
            }
            Some(Some(session)) => {
                Config::use_agent(config, agent, Some(session), abort_signal.clone()).await
            }
            None => Config::use_agent(config, agent, None, abort_signal.clone()).await,
        }
    }
    .await;

    // Local agents copy these values into their active Agent. Remote agents
    // have no local Agent object, so retain the values until their lazy NATS
    // metadata initialization snapshots them.
    if config.read().remote_agent.is_none() {
        config.write().agent_variables = None;
    }
    result
}

fn spawn_remote_session_cleanup(config: &GlobalConfig) {
    let Some(days) = config
        .read()
        .cleanup_remote_sessions_days
        .filter(|days| *days > 0)
    else {
        return;
    };
    let config = Arc::clone(config);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(3600));
        loop {
            interval.tick().await;
            let mut cluster_names = config
                .read()
                .nats_servers
                .iter()
                .map(|server| server.name.clone())
                .collect::<Vec<_>>();
            if !cluster_names
                .iter()
                .any(|name| name == harnx_runtime::config::LOCAL_CLUSTER_KEY)
            {
                cluster_names.push(harnx_runtime::config::LOCAL_CLUSTER_KEY.to_string());
            }
            for cluster_name in cluster_names {
                let snapshot = config.read().clone();
                let stats = run_remote_cleanup(&snapshot, days, &cluster_name).await;
                emit_remote_cleanup_summary(cluster_name, stats);
            }
        }
    });
}

async fn run(config: GlobalConfig, cli: Cli, text: Option<String>) -> Result<()> {
    let abort_signal = create_abort_signal();

    // Install a process-wide SIGINT watcher ONLY for one-shot (Cmd) mode:
    // set the abort flag that `eval_tool_calls` and sibling async sites
    // poll, letting the in-flight work exit cleanly with a non-zero status.
    // TUI has its own Ctrl-C path via the terminal; server processes run on a
    // separate thread with its own runtime — for it we let SIGINT use the
    // default handler (kill the process) so the parent sees a terminated
    // child within the expected window.
    let working_mode = config.read().working_mode.clone();
    if matches!(working_mode, WorkingMode::Cmd) {
        let abort_for_signal = abort_signal.clone();
        tokio::spawn(async move {
            while tokio::signal::ctrl_c().await.is_ok() {
                abort_for_signal.set_ctrlc();
            }
        });
    }

    if cli.sync_models {
        let url = config.read().sync_models_url();
        return Config::sync_models(&url, abort_signal.clone()).await;
    }

    if cli.list_models {
        for model in list_models(&config.read().clients, ModelType::Chat) {
            println!("{}", model.id());
        }
        return Ok(());
    }
    if cli.list_agents {
        let agents = list_agents().join("\n");
        println!("{agents}");
        return Ok(());
    }
    if cli.list_assistant_agents {
        let agents = list_assistant_agents().await.join("\n");
        println!("{agents}");
        return Ok(());
    }
    if cli.list_rags {
        let rags = Config::list_rags().join("\n");
        println!("{rags}");
        return Ok(());
    }
    if cli.list_macros {
        let macros = Config::list_macros().join("\n");
        println!("{macros}");
        return Ok(());
    }

    if cli.dry_run {
        config.write().dry_run = true;
    }

    if let Some(agent) = &cli.agent {
        activate_cli_agent(&config, &cli, agent, &abort_signal).await?;
    } else {
        if let Some(prompt) = &cli.prompt {
            config.write().use_prompt(prompt)?;
        }
        apply_session_arg(&config, &cli).await?;
        if let Some(rag) = &cli.rag {
            Config::use_rag(&config, Some(rag), abort_signal.clone()).await?;
        }
    }
    if let Some(model_id) = &cli.model {
        config.write().set_model(model_id)?;
    }
    if !cli.tool.is_empty() {
        let existing = config
            .read()
            .extract_agent()
            .use_tools()
            .unwrap_or_default();
        let mut tools: Vec<String> = existing;
        for t in &cli.tool {
            if !tools.iter().any(|v| v == t) {
                tools.push(t.clone());
            }
        }
        config.write().set_use_tools(Some(tools));
    }
    if cli.no_stream {
        config.write().stream = false;
    }
    if cli.empty_session {
        config.write().empty_session()?;
    }
    if cli.info {
        let info = config.read().info()?;
        println!("{info}");
        return Ok(());
    }

    // Spawn remote session cleanup background task if enabled.
    // MUST run before command/TUI branching so cleanup runs in all harnx modes.
    // The task is best-effort and never panics; deletions are fault-tolerant.
    spawn_remote_session_cleanup(&config);

    let is_tui = config.read().working_mode.is_tui();
    if cli.rebuild_rag {
        Config::rebuild_rag(&config, abort_signal.clone()).await?;
        if is_tui {
            return Ok(());
        }
    }
    if let Some(name) = &cli.macro_name {
        macro_execute(&config, name, text.as_deref(), abort_signal.clone()).await?;
        return Ok(());
    }
    match is_tui {
        false => run_one_shot(&config, &cli, text, abort_signal).await,
        true => {
            if !*IS_STDOUT_TERMINAL {
                bail!("No TTY for TUI")
            }
            start_interactive(&config).await
        }
    }
}

async fn run_one_shot(
    config: &GlobalConfig,
    cli: &Cli,
    text: Option<String>,
    abort_signal: AbortSignal,
) -> Result<()> {
    let (highlight, render_options) = {
        let cfg = config.read();
        (cfg.highlight, cfg.render_options().unwrap_or_default())
    };
    agent_event_sink::install_cli_agent_event_sink(
        highlight,
        render_options,
        abort_signal.clone(),
        cli.final_only,
    );
    if config.read().agent.is_none() {
        bail!("No agent selected. Use --agent/-a to specify an agent.");
    }
    if config.read().session.is_none() {
        let session_id = Config::reserve_new_session_id(config).await?;
        config.write().use_session(Some(&session_id))?;
    }
    let input = create_input(config, text, &cli.file, abort_signal.clone()).await?;
    let aborted_check = abort_signal.clone();
    let options = oneshot_nats::InvocationOptions::new(
        abort_signal,
        cli.final_only,
        cli.timeout_secs,
        cli.token_budget,
    )
    .with_resume_anyway(cli.resume_anyway);
    let result = start_directive(config, input, options).await;
    exit_session(config, !cli.final_only)?;
    match result {
        Err(error) if invocation_limit_reached(&error) => Err(error),
        _ if aborted_check.aborted() => bail!("interrupted by user"),
        result => result,
    }
}

fn session_resume_command(config: &GlobalConfig) -> Option<String> {
    let config_read = config.read();
    let session = config_read.session.as_ref()?;
    let session_name = session.id();

    let agent_name = config_read.agent.as_ref().map(|a| a.name());

    let mut args = vec!["harnx".to_string()];
    if let Some(agent) = agent_name {
        args.push("-a".to_string());
        args.push(agent.to_string());
    }
    args.push("-s".to_string());
    args.push(session_name.to_string());

    Some(shell_words::join(args))
}

fn source_heading(source: &AgentSource) -> String {
    source.heading()
}

fn one_shot_session_heading(final_only: bool, agent: &str, session_id: &str) -> Option<String> {
    (!final_only).then(|| {
        source_heading(&AgentSource {
            agent: agent.to_string(),
            session_id: Some(session_id.to_string()),
            model: None,
        })
    })
}

struct BreakdownSections<'a> {
    first_user: &'a str,
    last_user: Option<&'a str>,
    final_response: Vec<&'a str>,
}

fn transcript_item_text(item: &TranscriptItem) -> Option<&str> {
    match item {
        TranscriptItem::UserText { text, .. }
        | TranscriptItem::AssistantText { text, .. }
        | TranscriptItem::ThoughtText(text) => Some(text),
        _ => None,
    }
}

fn select_breakdown_sections(transcript: &[TranscriptItem]) -> Option<BreakdownSections<'_>> {
    let first_user_idx = transcript
        .iter()
        .position(|item| matches!(item, TranscriptItem::UserText { .. }))?;
    let last_user_idx = transcript
        .iter()
        .rposition(|item| matches!(item, TranscriptItem::UserText { .. }))?;

    let first_user = transcript_item_text(&transcript[first_user_idx])?;
    let last_user = (last_user_idx != first_user_idx)
        .then(|| transcript_item_text(&transcript[last_user_idx]))
        .flatten();
    let final_response = transcript
        .iter()
        .skip(last_user_idx + 1)
        .filter_map(|item| match item {
            TranscriptItem::AssistantText { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();

    Some(BreakdownSections {
        first_user,
        last_user,
        final_response,
    })
}

fn render_markdown_to_stderr(render: &mut MarkdownRender, text: &str) {
    if text.is_empty() {
        return;
    }
    eprintln!("{}", render.render(text));
}

fn print_session_breakdown(
    transcript: &[TranscriptItem],
    source: &AgentSource,
    config: &GlobalConfig,
) {
    let Some(sections) = select_breakdown_sections(transcript) else {
        return;
    };

    let render_options = config.read().render_options().unwrap_or_default();
    let Ok(mut render) = MarkdownRender::init(render_options) else {
        return;
    };

    eprintln!("{}", source_heading(source));
    render_markdown_to_stderr(&mut render, sections.first_user);

    if let Some(last_user) = sections.last_user {
        eprintln!("---");
        render_markdown_to_stderr(&mut render, last_user);
    }

    if !sections.final_response.is_empty() {
        eprintln!("---");
        for text in sections.final_response {
            render_markdown_to_stderr(&mut render, text);
        }
    }
}

fn exit_session(config: &GlobalConfig, show_resume_hint: bool) -> Result<()> {
    let resume_cmd = show_resume_hint
        .then(|| session_resume_command(config))
        .flatten();
    config.write().exit_session()?;

    if let Some(cmd) = resume_cmd {
        eprintln!(
            "\n{}\n  {}",
            dimmed_text("Resume this session by running:"),
            cmd
        );
    }

    Ok(())
}

async fn start_directive(
    config: &GlobalConfig,
    mut input: Input,
    options: oneshot_nats::InvocationOptions,
) -> Result<()> {
    crate::config::input::use_embeddings(&mut input, config, options.abort_signal().clone())
        .await?;

    let (agent, cluster, session_id) = {
        let cfg = config.read();
        let (agent, cluster) = cfg.remote_agent.clone().unwrap_or_else(|| {
            (
                cfg.agent
                    .as_ref()
                    .map(|agent| agent.name().to_string())
                    .unwrap_or_default(),
                harnx_runtime::config::LOCAL_CLUSTER_KEY.to_string(),
            )
        });
        let session_id = cfg.session.as_ref().map(|session| session.id().to_string());
        (agent, cluster, session_id)
    };

    let local_worker = tokio::sync::Mutex::new(None);
    let activation_route = harnx_runtime::local_orchestrator::activation_route_for_cluster(
        &cluster,
        &local_worker,
        options.abort_signal().clone(),
    )
    .await?;

    let initializer = {
        let config = config.read();
        harnx_runtime::SessionInitializer::named_from_config(agent.clone(), &config)
    };
    let session = harnx_runtime::NatsSession::from_global_config(
        harnx_runtime::NatsSessionConfig {
            cluster,
            initializer,
            session_id,
            activation_route,
        },
        config,
        options.abort_signal().clone(),
    )
    .await
    .context("failed to create NATS session")?;
    resume_session_anyway(&session, options.resume_anyway()).await?;
    if let Some(heading) =
        one_shot_session_heading(options.final_only(), &agent, session.session_id())
    {
        eprintln!("{heading}");
    }
    let sink = harnx_core::sink::current_agent_event_sink()
        .context("CLI agent event sink is not installed")?;
    let buffering_sink = Arc::new(harnx_runtime::InvocationBufferingSink::new(sink));
    let tracking_sink = Arc::new(oneshot_nats::AssistantTextTrackingSink::new(
        buffering_sink.clone(),
    ));
    let input_text = input.text();
    let result =
        oneshot_nats::run_turn(&session, &input_text, tracking_sink.clone(), &options).await?;
    oneshot_nats::finish_turn(
        result,
        oneshot_nats::TurnOutput {
            session_id: session.session_id(),
            buffering_sink: &buffering_sink,
            tracking_sink: &tracking_sink,
            options: &options,
        },
    )
}

async fn resume_session_anyway(session: &harnx_runtime::NatsSession, enabled: bool) -> Result<()> {
    if !enabled {
        return Ok(());
    }
    let Some(expected_execution_id) = session
        .execution_store()
        .current(session.session_id())
        .await?
        .map(|operation| operation.reference.execution_id)
    else {
        return Ok(());
    };
    let receipt = session
        .abandon_unconfirmed_cancellation(&expected_execution_id)
        .await
        .context("--resume-anyway could not abandon the pending cancellation")?;
    if receipt.abandoned {
        eprintln!(
            "Warning: resumed session '{}' by abandoning execution '{}'; prior work may still be running.",
            session.session_id(),
            receipt.execution_id.as_deref().unwrap_or("unknown"),
        );
    }
    Ok(())
}

async fn start_interactive(config: &GlobalConfig) -> Result<()> {
    let mut tui: Tui = Tui::init(config).await?;
    let result = tui.run().await;
    if let Some(details) = tui.exit_interrupt_error() {
        eprintln!("Warning: failed to interrupt active session while exiting ({details}).");
    }
    let source = {
        let cfg = config.read();
        AgentSource {
            agent: cfg.extract_agent().name().to_string(),
            session_id: cfg.session.as_ref().map(|s| s.id().to_string()),
            model: cfg.current_model_id(),
        }
    };
    print_session_breakdown(tui.transcript(), &source, config);
    exit_session(config, true)?;
    result
}

async fn create_input(
    config: &GlobalConfig,
    text: Option<String>,
    file: &[String],
    abort_signal: AbortSignal,
) -> Result<Input> {
    let input = if file.is_empty() {
        crate::config::input::from_str(config, &text.unwrap_or_default(), None)
    } else {
        crate::config::input::from_files_with_spinner(
            config,
            &text.unwrap_or_default(),
            file.to_vec(),
            None,
            abort_signal,
        )
        .await?
    };
    if input.is_empty() {
        bail!("No input");
    }
    Ok(input)
}

use harnx_core::logging::LogSink;
use harnx_runtime::bootstrap::setup_logger;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_shot_heading_identifies_root_agent_and_session() {
        assert_eq!(
            one_shot_session_heading(false, "coding/coder", "session-123").as_deref(),
            Some("> coding/coder ▸ session-123")
        );
    }

    #[test]
    fn final_only_suppresses_one_shot_heading() {
        assert_eq!(
            one_shot_session_heading(true, "coding/coder", "session-123"),
            None
        );
    }

    fn user_text(text: &str) -> TranscriptItem {
        TranscriptItem::UserText {
            text: text.to_string(),
            seq: None,
            timestamp: None,
        }
    }

    fn assistant_text(text: &str) -> TranscriptItem {
        TranscriptItem::AssistantText {
            text: text.to_string(),
            seq: None,
            timestamp: None,
            rendered_cache: None,
        }
    }

    fn thought_text(text: &str) -> TranscriptItem {
        TranscriptItem::ThoughtText(text.to_string())
    }

    fn system_text(text: &str) -> TranscriptItem {
        TranscriptItem::SystemText(text.to_string())
    }

    #[test]
    fn select_breakdown_sections_returns_none_for_empty_transcript() {
        assert!(select_breakdown_sections(&[]).is_none());
    }

    #[test]
    fn select_breakdown_sections_with_single_user_message_has_no_last_or_final_response() {
        let transcript = vec![user_text("hello")];

        let sections = select_breakdown_sections(&transcript).unwrap();

        assert_eq!(sections.first_user, "hello");
        assert_eq!(sections.last_user, None);
        assert!(sections.final_response.is_empty());
    }

    #[test]
    fn select_breakdown_sections_with_multiple_user_messages_sets_first_and_last() {
        let transcript = vec![
            user_text("first"),
            assistant_text("mid-response"),
            user_text("last"),
        ];

        let sections = select_breakdown_sections(&transcript).unwrap();

        assert_eq!(sections.first_user, "first");
        assert_eq!(sections.last_user, Some("last"));
        assert!(sections.final_response.is_empty());
    }

    #[test]
    fn select_breakdown_sections_collects_trailing_assistant_text_only() {
        let transcript = vec![
            user_text("question"),
            user_text("follow-up"),
            assistant_text("answer"),
            thought_text("thinking"),
        ];

        let sections = select_breakdown_sections(&transcript).unwrap();

        assert_eq!(sections.final_response, vec!["answer"]);
    }

    #[test]
    fn select_breakdown_sections_excludes_non_response_items_from_final_response() {
        let transcript = vec![
            user_text("question"),
            user_text("last prompt"),
            system_text("noise"),
            assistant_text("answer"),
            system_text("more noise"),
            thought_text("thinking"),
        ];

        let sections = select_breakdown_sections(&transcript).unwrap();

        assert_eq!(sections.final_response, vec!["answer"]);
    }

    #[test]
    fn select_breakdown_sections_with_immediate_exit_has_empty_final_response() {
        let transcript = vec![user_text("first"), user_text("last")];

        let sections = select_breakdown_sections(&transcript).unwrap();

        assert_eq!(sections.first_user, "first");
        assert_eq!(sections.last_user, Some("last"));
        assert!(sections.final_response.is_empty());
    }
}

#[cfg(test)]
mod resume_tests {
    use super::*;
    use harnx_runtime::config::session::Session;

    fn make_config(session: Option<Session>) -> GlobalConfig {
        let config = Config {
            session,
            ..Default::default()
        };
        Arc::new(RwLock::new(config))
    }

    fn session_with_message(id: &str) -> Session {
        let mut session = Session {
            id: id.to_string(),
            ..Default::default()
        };
        session.messages.push(crate::client::Message::default());
        session
    }

    #[test]
    fn returns_none_when_no_session() {
        let config = make_config(None);
        assert!(session_resume_command(&config).is_none());
    }

    #[test]
    fn returns_command_for_empty_reserved_session() {
        let config = make_config(Some(Session {
            id: "test".to_string(),
            ..Default::default()
        }));
        assert_eq!(session_resume_command(&config).unwrap(), "harnx -s test");
    }

    #[test]
    fn returns_command_for_plain_named_session() {
        let session = session_with_message("my-session");
        let config = make_config(Some(session));
        assert_eq!(
            session_resume_command(&config).unwrap(),
            "harnx -s my-session"
        );
    }

    #[test]
    fn includes_agent_when_set_in_session() {
        let session = session_with_message("my-session");
        let mut agent = crate::config::Agent::default();
        agent.set_name("my-agent");

        let config = Config {
            agent: Some(agent),
            session: Some(session),
            ..Default::default()
        };
        let config = Arc::new(RwLock::new(config));

        assert_eq!(
            session_resume_command(&config).unwrap(),
            "harnx -a my-agent -s my-session"
        );
    }

    #[test]
    fn returns_agent_and_session_in_resume_command() {
        // Test with UUID-like anonymous session and agent
        let session = session_with_message("550e8400-e29b-41d4-a716-446655440000");
        let mut agent = crate::config::Agent::default();
        agent.set_name("atlas");

        let config = Config {
            agent: Some(agent),
            session: Some(session),
            ..Default::default()
        };
        let config = Arc::new(RwLock::new(config));

        assert_eq!(
            session_resume_command(&config).unwrap(),
            "harnx -a atlas -s 550e8400-e29b-41d4-a716-446655440000"
        );
    }

    #[test]
    fn shell_quotes_names_with_spaces() {
        let session = session_with_message("my session");
        let mut agent = crate::config::Agent::default();
        agent.set_name("my agent");

        let config = Config {
            agent: Some(agent),
            session: Some(session),
            ..Default::default()
        };
        let config = Arc::new(RwLock::new(config));

        assert_eq!(
            session_resume_command(&config).unwrap(),
            "harnx -a 'my agent' -s 'my session'"
        );
    }
}

#[cfg(test)]
mod tests_list_sessions_routing {
    use super::*;

    fn session_meta(id: &str) -> SessionMeta {
        SessionMeta {
            id: id.to_string(),
            session_id: Some(id.to_string()),
            agent_name: None,
            title: None,
            modified: None,
            contexts: vec![],
        }
    }

    /// Routing decision: no remote agent → Local
    /// This test will fail if routing logic regresses to unconditionally use remote.
    #[test]
    fn test_routing_local_when_no_remote_agent() {
        let target = resolve_list_sessions_target(None);
        assert_eq!(target, ListSessionsTarget::Local);
    }

    /// Routing decision: remote agent set → Remote with correct cluster
    /// This test will fail if routing logic regresses to unconditionally use local
    /// or if cluster extraction is broken.
    #[test]
    fn test_routing_remote_when_remote_agent_set() {
        let remote_agent = Some(("my-agent".to_string(), "my-cluster".to_string()));
        let target = resolve_list_sessions_target(remote_agent.as_ref());
        assert_eq!(
            target,
            ListSessionsTarget::Remote {
                cluster: "my-cluster".to_string()
            }
        );
    }

    /// Routing decision: cluster extraction preserves full cluster name
    #[test]
    fn test_routing_remote_extracts_cluster_correctly() {
        let test_cases = [
            ("agent", "nats://localhost:4222"),
            ("worker", "production-cluster"),
            ("remote-agent", "cluster.with.dots.example.com"),
        ];

        for (agent, cluster) in test_cases {
            let remote_agent = Some((agent.to_string(), cluster.to_string()));
            let target = resolve_list_sessions_target(remote_agent.as_ref());
            match target {
                ListSessionsTarget::Remote { cluster: extracted } => {
                    assert_eq!(extracted, cluster, "cluster mismatch for agent '{agent}'");
                }
                ListSessionsTarget::Local => {
                    panic!("Expected Remote target for agent '{agent}', got Local");
                }
            }
        }
    }

    /// Output formatting: one session ID per line
    /// This test will fail if the formatting changes (e.g., comma-separated).
    #[test]
    fn test_output_format_one_id_per_line() {
        let sessions = [session_meta("session-1"), session_meta("session-2")];
        let output = format_sessions_for_output(&sessions);
        assert_eq!(output, "session-1\nsession-2");
    }

    /// Output formatting: empty sessions → empty string
    #[test]
    fn test_output_format_empty_sessions() {
        let sessions: Vec<SessionMeta> = vec![];
        let output = format_sessions_for_output(&sessions);
        assert_eq!(output, "");
    }

    /// Output formatting: single session → single line (no trailing newline)
    #[test]
    fn test_output_format_single_session() {
        let sessions = [session_meta("only-session")];
        let output = format_sessions_for_output(&sessions);
        assert_eq!(output, "only-session");
    }

    /// Remote list outcome: empty sessions → Print("") (not an error)
    /// Regression guard: empty result is valid output, not an error condition.
    #[test]
    fn test_remote_list_outcome_empty_ok() {
        let result: Result<Vec<SessionMeta>, anyhow::Error> = Ok(vec![]);
        let outcome = remote_list_outcome(result);
        assert_eq!(outcome, ListSessionsOutcome::Print(String::new()));
    }

    /// Remote list outcome: Ok with sessions → Print with one id per line
    #[test]
    fn test_remote_list_outcome_ok_with_sessions() {
        let sessions = vec![session_meta("sess-a"), session_meta("sess-b")];
        let result: Result<Vec<SessionMeta>, anyhow::Error> = Ok(sessions);
        let outcome = remote_list_outcome(result);
        assert_eq!(
            outcome,
            ListSessionsOutcome::Print("sess-a\nsess-b".to_string())
        );
    }

    /// Remote list outcome: Err → Error (error is surfaced, NOT swallowed).
    /// Regression guard: if this test fails, it means errors are being silently
    /// converted to empty-success (the bug we're preventing).
    #[test]
    fn test_remote_list_outcome_error() {
        let error = anyhow::anyhow!("connection refused");
        let result: Result<Vec<SessionMeta>, anyhow::Error> = Err(error);
        let outcome = remote_list_outcome(result);
        // Critical: must be Error variant, NOT Print("")
        match outcome {
            ListSessionsOutcome::Error(msg) => {
                assert!(
                    msg.contains("connection refused"),
                    "error message preserved"
                );
            }
            ListSessionsOutcome::Print(_) => {
                panic!("regression: error was swallowed into Print variant!");
            }
        }
    }
}

/// Emit remote cleanup summary if any work was done.
/// Logs per-cluster summary for server visibility.
fn emit_remote_cleanup_summary(cluster: String, stats: RemoteCleanupStats) {
    if stats == RemoteCleanupStats::default() {
        return;
    }
    log::info!(
        "Remote session cleanup ({}): scanned={}, deleted={}, skipped_active={}, errors={}",
        cluster,
        stats.scanned,
        stats.deleted,
        stats.skipped_active,
        stats.errors
    );
}
