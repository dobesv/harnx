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
    Cli, Commands, DeleteSessionArgs, InfoSubcommands, SessionSubcommands, WorkerArgs,
};
use crate::client::{list_models, retry::call_with_retry_and_fallback, ModelType};
use crate::config::{
    list_agents, list_assistant_agents, load_env_file, macro_execute, render_agent_dump,
    render_session_dump, Config, GlobalConfig, Input, WorkingMode,
};
use crate::tui::{TranscriptItem, Tui};
use harnx_core::agent_config::collect_agent_variables;
use harnx_core::event::{AgentEvent, AgentSource, NoticeEvent};
use harnx_render::{render_error, MarkdownRender};
use harnx_runtime::config::SessionMeta;
use harnx_runtime::utils::*;

use anyhow::{bail, Context, Result};
use clap::Parser;
use parking_lot::RwLock;
use std::{sync::Arc, time::Duration};

use harnx_core::sink::emit_agent_event;
use harnx_runtime::remote_session_cleanup::{run_remote_cleanup, RemoteCleanupStats};
use harnx_runtime::session_cleanup::{humanize_bytes, run_cleanup, CleanupStats};

/// Routing decision for `--list-sessions` handler.
/// Extracted as a pure function for testability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListSessionsTarget {
    /// List sessions from the local session directory.
    Local,
    /// List sessions from a remote NATS cluster.
    Remote { cluster: String },
}

/// Pure routing decision for `--list-sessions`.
/// Given the remote agent context, returns whether to list local or remote sessions.
///
/// This function encapsulates the branch selection logic so it can be unit-tested
/// without requiring a live NATS cluster or mocking async I/O.
pub fn resolve_list_sessions_target(remote_agent: Option<&(String, String)>) -> ListSessionsTarget {
    match remote_agent {
        Some((_, cluster)) => ListSessionsTarget::Remote {
            cluster: cluster.clone(),
        },
        None => ListSessionsTarget::Local,
    }
}

/// Format session metadata as one ID per line.
/// This helper is extracted for testability without touching stdout.
pub fn format_sessions_for_output(sessions: &[SessionMeta]) -> String {
    let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    ids.join("\n")
}

/// Outcome of a remote list-sessions operation.
/// This pure helper enables unit-testing the error-propagation path without
/// mocking the async NATS client or relying on assertion side-effects.
#[derive(Debug, PartialEq, Eq)]
pub enum ListSessionsOutcome {
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
pub fn remote_list_outcome(result: Result<Vec<SessionMeta>, anyhow::Error>) -> ListSessionsOutcome {
    match result {
        Ok(sessions) => ListSessionsOutcome::Print(format_sessions_for_output(&sessions)),
        Err(e) => ListSessionsOutcome::Error(format!("{e:#}")),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    load_env_file()?;
    let cli = Cli::parse();
    if let Some(command) = &cli.command {
        setup_logger(false)?;
        harnx_core::alloc_guard::init_from_env();
        return run_command(command).await;
    }

    let text = cli.text()?;
    let working_mode = if text.is_none() && cli.file.is_empty() {
        WorkingMode::Tui
    } else {
        WorkingMode::Cmd
    };
    let info_flag = legacy_info_flag(&cli);
    setup_logger(false)?;
    harnx_core::alloc_guard::init_from_env();
    let config = Arc::new(RwLock::new(Config::init(working_mode, info_flag).await?));
    if let Err(err) = run(config, cli, text).await {
        render_error(err);
        std::process::exit(1);
    }
    Ok(())
}

async fn run_command(command: &Commands) -> Result<()> {
    match command {
        Commands::Info(info_args) => match &info_args.command {
            InfoSubcommands::Agent { name } => {
                let config = Config::init(WorkingMode::Cmd, true).await?;
                let out = render_agent_dump(&config, name)?;
                println!("{out}");
                Ok(())
            }
            InfoSubcommands::Session {
                agent_name,
                session_id,
            } => {
                let out = render_session_dump(Some(agent_name.as_str()), session_id)?;
                println!("{out}");
                Ok(())
            }
        },
        Commands::Session(session_args) => match &session_args.command {
            SessionSubcommands::Delete(delete_args) => {
                run_session_delete_command(delete_args).await
            }
        },
        Commands::Worker(worker_args) => run_worker_command(worker_args).await,
    }
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
            "Deleted remote session '{}' on cluster '{}' (stream_deleted={}, lease_deleted={})",
            delete_args.session_id,
            delete_args.cluster,
            result.stream_deleted,
            result.lease_deleted
        );
    } else {
        println!(
            "Remote session '{}' on cluster '{}' not found; nothing to delete.",
            delete_args.session_id, delete_args.cluster
        );
    }

    Ok(())
}

async fn run_worker_command(worker_args: &WorkerArgs) -> Result<()> {
    let config = Arc::new(RwLock::new(Config::init(WorkingMode::Cmd, true).await?));
    config.write().agent_variables = collect_agent_variables(&worker_args.agent_variable)?;
    let worker_id = worker_args
        .worker_id
        .clone()
        .unwrap_or_else(harnx_runtime::nats_worker::new_remote_session_id);
    let daemon =
        harnx_runtime::nats_worker::WorkerDaemonConfig::new(worker_args.cluster.clone(), worker_id);
    let call_fn: harnx_runtime::agent_loop::AgentCallFn =
        std::sync::Arc::new(|input, config, abort| {
            Box::pin(call_with_retry_and_fallback(input, config, abort))
        });
    harnx_runtime::nats_worker::run_worker_daemon(config, daemon, Some(call_fn)).await
}

fn legacy_info_flag(cli: &Cli) -> bool {
    cli.info
        || cli.sync_models
        || cli.list_models
        || cli.list_agents
        || cli.list_assistant_agents
        || cli.list_rags
        || cli.list_macros
        || cli.list_sessions
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
        // cli.session is Option<Option<String>>:
        //   None          → --session not provided; no session
        //   Some(None)    → bare --session flag; generate a new session ID
        //   Some(Some(s)) → --session <id>; use that ID
        let generated_session_id;
        let session = match &cli.session {
            None => None,
            Some(None) => {
                generated_session_id = config.read().new_session_id()?;
                Some(generated_session_id.as_str())
            }
            Some(Some(s)) => Some(s.as_str()),
        };
        config.write().agent_variables = collect_agent_variables(&cli.agent_variable)?;

        let ret = Config::use_agent(&config, agent, session, abort_signal.clone()).await;
        config.write().agent_variables = None;
        ret?;
    } else {
        if let Some(prompt) = &cli.prompt {
            config.write().use_prompt(prompt)?;
        }
        if let Some(session) = &cli.session {
            config
                .write()
                .use_session(session.as_ref().map(|v| v.as_str()))?;
        }
        if let Some(rag) = &cli.rag {
            Config::use_rag(&config, Some(rag), abort_signal.clone()).await?;
        }
    }
    if cli.list_sessions {
        // Use the pure routing decision function for branch selection.
        // This logic is now unit-testable via resolve_list_sessions_target.
        let target = resolve_list_sessions_target(config.read().remote_agent.as_ref());
        match target {
            ListSessionsTarget::Remote { cluster } => {
                // Clone config to avoid holding lock across await
                let cfg = config.read().clone();
                let result = cfg.list_remote_sessions_with_meta(&cluster).await;
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
                let sessions = config.read().list_sessions().join("\n");
                println!("{sessions}");
            }
        }
        return Ok(());
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
    if cli.save_session {
        config.write().set_save_session_this_time()?;
    }
    if cli.info {
        let info = config.read().info()?;
        println!("{info}");
        return Ok(());
    }

    // Spawn session cleanup background task if enabled.
    // MUST run before command/TUI branching so cleanup runs in all harnx modes.
    // The task is best-effort and never panics; deletions are fault-tolerant.
    let cleanup_days = config.read().cleanup_inactive_sessions_days;
    if let Some(days) = cleanup_days {
        if days > 0 {
            let config_clone = Arc::clone(&config);
            tokio::spawn(async move {
                // tokio::time::interval fires its first tick immediately, so cleanup runs
                // once at startup and then every hour thereafter.
                let mut interval = tokio::time::interval(Duration::from_secs(3600));
                loop {
                    interval.tick().await;
                    let stats = run_cleanup(&config_clone, days).await;
                    emit_cleanup_summary(stats);
                }
            });
        }
    }

    // Spawn remote session cleanup background task if enabled.
    // MUST run before command/TUI branching so cleanup runs in all harnx modes.
    // The task is best-effort and never panics; deletions are fault-tolerant.
    let remote_cleanup_days = config.read().cleanup_remote_sessions_days;
    if let Some(days) = remote_cleanup_days {
        if days > 0 {
            let config_clone = Arc::clone(&config);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(3600));
                loop {
                    interval.tick().await;
                    // Collect cluster names up front to release config lock before any await.
                    let cluster_names: Vec<String> = {
                        config_clone
                            .read()
                            .nats_servers
                            .iter()
                            .map(|s| s.name.clone())
                            .collect()
                    };
                    for cluster_name in cluster_names {
                        // Clone config to avoid holding lock across await.
                        // Config is cheap to clone (Arc-wrapped internally).
                        let config_snapshot = config_clone.read().clone();
                        let stats = run_remote_cleanup(&config_snapshot, days, &cluster_name).await;
                        emit_remote_cleanup_summary(cluster_name, stats);
                    }
                }
            });
        }
    }

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
        false => {
            let (highlight, render_options) = {
                let cfg = config.read();
                (cfg.highlight, cfg.render_options().unwrap_or_default())
            };
            agent_event_sink::install_cli_agent_event_sink(
                highlight,
                render_options,
                abort_signal.clone(),
            );
            {
                let cfg = config.read();
                if cfg.agent.is_none() {
                    bail!("No agent selected. Use --agent/-a to specify an agent.");
                }
            }
            {
                let mut cfg = config.write();
                if cfg.session.is_none() {
                    use harnx_runtime::config::{build_picker_context, find_matching_session};
                    let sessions = cfg.list_sessions_with_meta();
                    let ctx = build_picker_context(None);
                    let agent_name = cfg
                        .agent
                        .as_ref()
                        .map(|a| a.name().to_string())
                        .unwrap_or_default();
                    let matching_id = find_matching_session(&sessions, &ctx, &agent_name);
                    if let Some(ref id) = matching_id {
                        eprintln!("{}", dimmed_text(&format!("Resuming session {id}")));
                    }
                    cfg.use_session(matching_id.as_deref())?;
                }
            }
            let input = create_input(&config, text, &cli.file, abort_signal.clone()).await?;
            let aborted_check = abort_signal.clone();
            let result = start_directive(&config, input, abort_signal).await;
            exit_session(&config)?;
            if aborted_check.aborted() {
                bail!("interrupted by user");
            }
            result
        }
        true => {
            if !*IS_STDOUT_TERMINAL {
                bail!("No TTY for TUI")
            }
            start_interactive(&config).await
        }
    }
}

fn session_resume_command(config: &GlobalConfig) -> Option<String> {
    let config_read = config.read();
    let session = config_read.session.as_ref()?;
    if session.is_empty() {
        return None;
    }

    let save_session = session.save_session;
    let save_session_this_time = session.save_session_this_time;
    if save_session == Some(false) && !save_session_this_time {
        return None;
    }

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

fn exit_session(config: &GlobalConfig) -> Result<()> {
    let resume_cmd = session_resume_command(config);
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

#[async_recursion::async_recursion]
async fn start_directive(
    config: &GlobalConfig,
    input: Input,
    abort_signal: AbortSignal,
) -> Result<()> {
    start_directive_inner(config, input, abort_signal, 0, true).await
}

#[allow(clippy::too_many_arguments)]
async fn start_directive_inner(
    config: &GlobalConfig,
    mut input: Input,
    abort_signal: AbortSignal,
    _resume_count: u32,
    with_embeddings: bool,
) -> Result<()> {
    if with_embeddings {
        crate::config::input::use_embeddings(&mut input, config, abort_signal.clone()).await?;
    }

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

    let mut local_worker = None;
    if cluster == harnx_runtime::config::LOCAL_CLUSTER_KEY {
        harnx_runtime::local_orchestrator::ensure_local_worker(&mut local_worker)
            .await
            .context("failed to ensure local NATS worker")?;
    }

    let session = harnx_runtime::ThinClientSession::from_global_config(
        harnx_runtime::ThinClientConfig {
            cluster,
            agent,
            session_id,
        },
        config,
        abort_signal,
    )
    .await
    .context("failed to create thin-client session")?;
    let sink = harnx_core::sink::current_agent_event_sink()
        .context("CLI agent event sink is not installed")?;
    let tracking_sink = Arc::new(oneshot_nats::AssistantTextTrackingSink::new(sink));
    let result = session
        .run_turn(&input.text(), tracking_sink.clone(), None)
        .await?;

    let worker_error = result.error.clone();
    tracking_sink.emit_durable_response_if_needed(result);
    // A worker-side failure must not exit 0 — scripts driving one-shot mode
    // rely on the exit code to tell an answer from a dead turn.
    match worker_error {
        Some(error) => Err(anyhow::anyhow!(error)),
        None => Ok(()),
    }
}

async fn start_interactive(config: &GlobalConfig) -> Result<()> {
    let mut tui: Tui = Tui::init(config).await?;
    let result = tui.run().await;
    let source = {
        let cfg = config.read();
        AgentSource {
            agent: cfg.extract_agent().name().to_string(),
            session_id: cfg.session.as_ref().map(|s| s.id().to_string()),
            model: cfg.current_model_id(),
        }
    };
    print_session_breakdown(tui.transcript(), &source, config);
    exit_session(config)?;
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

use harnx_runtime::bootstrap::setup_logger;

#[cfg(test)]
mod tests {
    use super::*;

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
    fn returns_none_for_empty_session() {
        let config = make_config(Some(Session {
            id: "test".to_string(),
            ..Default::default()
        }));
        assert!(session_resume_command(&config).is_none());
    }

    #[test]
    fn returns_none_when_save_session_false() {
        let mut session = session_with_message("test");
        session.save_session = Some(false);
        let config = make_config(Some(session));
        assert!(session_resume_command(&config).is_none());
    }

    #[test]
    fn returns_command_when_save_session_false_but_save_this_time() {
        let mut session = session_with_message("test");
        session.save_session = Some(false);
        session.save_session_this_time = true;
        let config = make_config(Some(session));
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
        let sessions = [
            SessionMeta {
                id: "session-1".to_string(),
                session_id: Some("session-1".to_string()),
                working_dir: None,
                git_branch: None,
                git_remote: None,
                terminal_session_id: None,
                agent_name: None,
                title: None,
                modified: None,
            },
            SessionMeta {
                id: "session-2".to_string(),
                session_id: Some("session-2".to_string()),
                working_dir: None,
                git_branch: None,
                git_remote: None,
                terminal_session_id: None,
                agent_name: None,
                title: None,
                modified: None,
            },
        ];
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
        let sessions = [SessionMeta {
            id: "only-session".to_string(),
            session_id: Some("only-session".to_string()),
            working_dir: None,
            git_branch: None,
            git_remote: None,
            terminal_session_id: None,
            agent_name: None,
            title: None,
            modified: None,
        }];
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
        let sessions = vec![
            SessionMeta {
                id: "sess-a".to_string(),
                session_id: Some("sess-a".to_string()),
                working_dir: None,
                git_branch: None,
                git_remote: None,
                terminal_session_id: None,
                agent_name: None,
                title: None,
                modified: None,
            },
            SessionMeta {
                id: "sess-b".to_string(),
                session_id: Some("sess-b".to_string()),
                working_dir: None,
                git_branch: None,
                git_remote: None,
                terminal_session_id: None,
                agent_name: None,
                title: None,
                modified: None,
            },
        ];
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

/// Emit cleanup summary if sessions were removed.
/// `emit_agent_event` buffers the event until a TUI/CLI sink is installed,
/// then replays it to transcript.
///
/// We also log summary directly so background cleanup remains visible even when
/// no interactive transcript sink is attached yet.
fn emit_cleanup_summary(stats: CleanupStats) {
    if stats.sessions_removed == 0 {
        return;
    }
    let msg = format!(
        "Note: cleaned up {} old sessions, {} disk freed",
        stats.sessions_removed,
        humanize_bytes(stats.bytes_freed)
    );
    // Always emit via agent event sink for transcript visibility.
    // Function returns true if event was delivered or buffered.
    emit_agent_event(AgentEvent::Notice(NoticeEvent::Info(msg.clone())));
    // Also log so early/background cleanup work is visible before sink attach.
    // In interactive use this can duplicate transcript text in logs.
    log::info!("{msg}");
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
