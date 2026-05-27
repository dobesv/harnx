mod agent_event_sink;
mod cli;
mod cli_event_sink;
mod serve;

#[macro_use]
extern crate log;

#[cfg(test)]
pub mod test_utils;

pub use harnx_mcp as mcp;
pub use harnx_mcp::safety as mcp_safety;
pub use harnx_runtime::{client, commands, config, tool};
pub use harnx_tui as tui;

use crate::cli::Cli;
use crate::client::{list_models, retry::call_with_retry_and_fallback, ModelType};
use crate::config::{
    list_agents, list_assistant_agents, load_env_file, macro_execute, Config, GlobalConfig, Input,
    WorkingMode,
};
use crate::tui::{TranscriptItem, Tui};
use harnx_core::event::AgentSource;
use harnx_hooks::{
    dispatch_hooks_with_count_and_manager, dispatch_hooks_with_managers, drain_async_results,
    inject_pending_async_context, AsyncHookManager, HookEvent, HookResultControl,
    PersistentHookManager,
};
use harnx_render::{render_error, MarkdownRender};
use harnx_runtime::utils::*;

use anyhow::{bail, Result};
use clap::Parser;
use parking_lot::RwLock;
use std::{env, path::PathBuf, sync::Arc, time::Duration};

#[tokio::main]
async fn main() -> Result<()> {
    load_env_file()?;
    let cli = Cli::parse();
    let text = if cli.should_read_stdin() {
        cli.text()?
    } else {
        None
    };
    let working_mode = if let Some(ref agent_name) = cli.acp {
        WorkingMode::Acp(agent_name.clone())
    } else if cli.serve.is_some() {
        WorkingMode::Serve
    } else if text.is_none() && cli.file.is_empty() {
        WorkingMode::Tui
    } else {
        WorkingMode::Cmd
    };
    let info_flag = cli.info
        || cli.sync_models
        || cli.list_models
        || cli.list_agents
        || cli.list_assistant_agents
        || cli.list_rags
        || cli.list_macros
        || cli.list_sessions;
    setup_logger(working_mode.is_serve() || working_mode.is_acp())?;
    let config = Arc::new(RwLock::new(
        Config::init(working_mode, info_flag, cli.mcp_root.clone()).await?,
    ));
    if let Err(err) = run(config, cli, text).await {
        render_error(err);
        std::process::exit(1);
    }
    Ok(())
}

async fn run(config: GlobalConfig, cli: Cli, text: Option<String>) -> Result<()> {
    let abort_signal = create_abort_signal();

    // Install a process-wide SIGINT watcher ONLY for one-shot (Cmd) mode:
    // set the abort flag that `eval_tool_calls` and sibling async sites
    // poll, letting the in-flight work exit cleanly with a non-zero status.
    // TUI has its own Ctrl-C path via the terminal; ACP server runs on a
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
        if !cli.agent_variable.is_empty() {
            config.write().agent_variables = Some(
                cli.agent_variable
                    .chunks(2)
                    .map(|v| (v[0].to_string(), v[1].to_string()))
                    .collect(),
            );
        }

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
        let sessions = config.read().list_sessions().join("\n");
        println!("{sessions}");
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
    let working_mode = config.read().working_mode.clone();
    if let WorkingMode::Acp(agent_name) = working_mode {
        return harnx_acp_server::run(config, agent_name).await;
    }
    if let Some(addr) = cli.serve {
        return serve::run(config, addr).await;
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
                    let ctx = build_picker_context();
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
            let mut async_manager = AsyncHookManager::new();
            let persistent_manager =
                Arc::new(tokio::sync::Mutex::new(PersistentHookManager::new()));
            let mut pending_async_context = None;
            dispatch_session_start(&config, "cmd", &async_manager, &persistent_manager).await;
            let aborted_check = abort_signal.clone();
            let result = start_directive(
                &config,
                input,
                abort_signal,
                &mut async_manager,
                &persistent_manager,
                &mut pending_async_context,
            )
            .await;
            exit_session_with_hook(&config, &async_manager, &persistent_manager).await?;
            persistent_manager.lock().await.shutdown();
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

fn hook_dispatch_context(config: &GlobalConfig) -> (harnx_hooks::HooksConfig, String, PathBuf) {
    let config = config.read();
    (
        config.resolved_hooks(),
        config
            .session
            .as_ref()
            .map(|session| session.id())
            .unwrap_or("default")
            .to_string(),
        env::current_dir().unwrap_or_default(),
    )
}

async fn dispatch_session_start(
    config: &GlobalConfig,
    source: &str,
    async_manager: &AsyncHookManager,
    persistent_manager: &Arc<tokio::sync::Mutex<PersistentHookManager>>,
) {
    let (hooks, session_id, cwd) = hook_dispatch_context(config);
    let model_id = config.read().current_model().id().to_string();
    let event = HookEvent::SessionStart {
        source: source.to_string(),
        model: model_id,
    };
    let _ = dispatch_hooks_with_managers(
        &event,
        &hooks.entries,
        &session_id,
        &cwd,
        Some(async_manager),
        Some(persistent_manager),
    )
    .await;
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

async fn exit_session_with_hook(
    config: &GlobalConfig,
    async_manager: &AsyncHookManager,
    persistent_manager: &Arc<tokio::sync::Mutex<PersistentHookManager>>,
) -> Result<()> {
    let resume_cmd = session_resume_command(config);
    let (hooks, session_id, cwd) = hook_dispatch_context(config);
    config.write().exit_session()?;
    let event = HookEvent::SessionEnd {
        reason: "session_exit".to_string(),
    };
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        dispatch_hooks_with_managers(
            &event,
            &hooks.entries,
            &session_id,
            &cwd,
            Some(async_manager),
            Some(persistent_manager),
        ),
    )
    .await;

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
    async_manager: &mut AsyncHookManager,
    persistent_manager: &Arc<tokio::sync::Mutex<PersistentHookManager>>,
    pending_async_context: &mut Option<String>,
) -> Result<()> {
    start_directive_inner(
        config,
        input,
        abort_signal,
        async_manager,
        persistent_manager,
        pending_async_context,
        0,
        true,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
#[async_recursion::async_recursion]
async fn start_directive_inner(
    config: &GlobalConfig,
    mut input: Input,
    abort_signal: AbortSignal,
    async_manager: &mut AsyncHookManager,
    persistent_manager: &Arc<tokio::sync::Mutex<PersistentHookManager>>,
    pending_async_context: &mut Option<String>,
    resume_count: u32,
    with_embeddings: bool,
) -> Result<()> {
    if with_embeddings {
        crate::config::input::use_embeddings(&mut input, config, abort_signal.clone()).await?;
    }
    drain_async_results(async_manager, pending_async_context);
    inject_pending_async_context(&mut input, pending_async_context);
    config.write().before_chat_completion(&input)?;
    let (hooks, session_id, cwd) = hook_dispatch_context(config);
    let input_text = input.text();
    let event = HookEvent::UserPromptSubmit {
        prompt: input_text.clone(),
    };
    let outcome = dispatch_hooks_with_count_and_manager(
        &event,
        &hooks.entries,
        &session_id,
        &cwd,
        resume_count,
        Some(async_manager),
        Some(persistent_manager),
    )
    .await;
    match outcome.control {
        HookResultControl::Block { reason } => bail!("{reason}"),
        HookResultControl::Ask { .. } => {} // Ask is not applicable for UserPromptSubmit
        HookResultControl::Continue => {}
    }
    let (output, thought, tool_calls, usage) =
        match call_with_retry_and_fallback(&input, config, abort_signal.clone()).await {
            Ok(result) => result,
            Err(err) => {
                let event = HookEvent::StopFailure {
                    error: err.to_string(),
                    error_type: "api_error".to_string(),
                };
                let _ = dispatch_hooks_with_managers(
                    &event,
                    &hooks.entries,
                    &session_id,
                    &cwd,
                    Some(async_manager),
                    Some(persistent_manager),
                )
                .await;
                let _ = config.write().after_chat_completion(
                    &input,
                    "",
                    None,
                    &[],
                    &Default::default(),
                );
                return Err(err);
            }
        };

    // Tool rounds use `execute_tool_round` to persist the request,
    // run the tools, and persist the results as two separate log
    // entries.  Plain-text rounds save once via after_chat_completion.
    let tool_results: Vec<harnx_runtime::tool::ToolResult> = if tool_calls.is_empty() {
        config
            .write()
            .after_chat_completion(&input, &output, thought.as_deref(), &[], &usage)?;
        Vec::new()
    } else {
        config.write().record_completion_usage(&usage);
        harnx_runtime::tool::execute_tool_round(
            config,
            &input,
            &harnx_runtime::tool::CompletionText {
                output: &output,
                thought: thought.as_deref(),
            },
            tool_calls,
            &abort_signal,
            persistent_manager,
        )
        .await?
    };
    if tool_results.is_empty() {
        let config_read = config.read();
        let status = config_read.render_status_line(true);
        let session_usage = config_read
            .session
            .as_ref()
            .map(|s| s.completion_usage().clone());
        let display_usage = session_usage.as_ref().unwrap_or(&usage);
        let context_stats = config_read
            .session
            .as_ref()
            .map(|s| {
                let (tokens, percent) = s.tokens_usage();
                if percent > 0.0 {
                    format!("💬 {}({:.0}%)", tokens, percent)
                } else {
                    format!("💬 {}", tokens)
                }
            })
            .unwrap_or_default();
        let mut line_parts = vec![];
        if !status.is_empty() {
            line_parts.push(status);
        }
        if !display_usage.is_empty() {
            line_parts.push(format!("   {}", display_usage));
        }
        if !context_stats.is_empty() {
            line_parts.push(format!("  {}", context_stats));
        }
        if !line_parts.is_empty() {
            eprintln!("{}", dimmed_text(&line_parts.join("")));
        }
        drop(config_read);
    }
    let stop_outcome = if tool_results.is_empty() {
        let event = HookEvent::Stop {
            stop_hook_active: true,
            last_assistant_message: Some(output.clone()),
        };
        let stop_outcome = dispatch_hooks_with_count_and_manager(
            &event,
            &hooks.entries,
            &session_id,
            &cwd,
            resume_count,
            Some(async_manager),
            Some(persistent_manager),
        )
        .await;
        if let Some(additional_context) = stop_outcome
            .result
            .additional_context
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            debug!(
                "Captured Stop hook additional context for later auto-continue in CMD: {additional_context}"
            );
        }
        Some(stop_outcome)
    } else {
        None
    };

    if !tool_results.is_empty() {
        let switch_agent = tool_results.iter().find_map(|v| v.switch_agent.clone());
        if let Some(switch_agent) = switch_agent {
            let merged_input = input.merge_tool_results(output, thought, tool_results.clone());
            config.write().exit_agent()?;
            crate::config::Config::use_agent(
                config,
                &switch_agent.agent,
                switch_agent.session_id.as_deref(),
                abort_signal.clone(),
            )
            .await?;
            // Always empty the session on handoff so the new agent starts
            // fresh — the prior agent's system prompt and messages should
            // not bleed into the new agent's session (#291).
            if config.read().session.is_some() {
                config.write().empty_session()?;
            }
            return Box::pin(start_directive_inner(
                config,
                merged_input,
                abort_signal,
                async_manager,
                persistent_manager,
                pending_async_context,
                0,
                true,
            ))
            .await;
        }

        return start_directive_inner(
            config,
            input.merge_tool_results(output, thought, tool_results),
            abort_signal,
            async_manager,
            persistent_manager,
            pending_async_context,
            resume_count,
            false,
        )
        .await;
    }

    if let Some(stop_outcome) = stop_outcome {
        let max_resume = hooks.max_resume.unwrap_or(5);
        if stop_outcome.result.resume.unwrap_or(false) && resume_count < max_resume {
            if abort_signal.aborted() {
                return Ok(());
            }

            let context = stop_outcome
                .result
                .additional_context
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "Continue working on pending tasks.".to_string());
            let new_input = crate::config::input::from_str(config, &context, None);
            return start_directive_inner(
                config,
                new_input,
                abort_signal,
                async_manager,
                persistent_manager,
                pending_async_context,
                resume_count + 1,
                true,
            )
            .await;
        }
    }

    let max_resume = hooks.max_resume.unwrap_or(5);
    if drain_async_results(async_manager, pending_async_context) && resume_count < max_resume {
        if abort_signal.aborted() {
            return Ok(());
        }

        let context = pending_async_context
            .take()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "Continue working on pending tasks.".to_string());
        let new_input = crate::config::input::from_str(config, &context, None);
        return start_directive_inner(
            config,
            new_input,
            abort_signal,
            async_manager,
            persistent_manager,
            pending_async_context,
            resume_count + 1,
            true,
        )
        .await;
    }

    Ok(())
}

async fn start_interactive(config: &GlobalConfig) -> Result<()> {
    let async_manager = AsyncHookManager::new();
    let persistent_manager = Arc::new(tokio::sync::Mutex::new(PersistentHookManager::new()));
    dispatch_session_start(config, "tui", &async_manager, &persistent_manager).await;
    let mut tui: Tui = Tui::init(config, async_manager, persistent_manager.clone()).await?;
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
    let async_manager = tui.async_manager().lock().await;
    exit_session_with_hook(config, &async_manager, &persistent_manager).await?;
    persistent_manager.lock().await.shutdown();
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
