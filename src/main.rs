mod acp;
mod cli;
mod client;
mod config;
mod hooks;
mod mcp;
pub mod mcp_safety;
mod rag;
mod render;
#[allow(dead_code, unused_imports)]
mod repl;
mod serve;
mod tool;
mod tui;
mod ui_output;
#[macro_use]
mod utils;

#[macro_use]
extern crate log;

#[cfg(test)]
pub mod test_utils;

use crate::cli::Cli;
use crate::client::{list_models, retry::call_with_retry_and_fallback, ModelType};
use crate::config::{
    ensure_parent_exists, list_agents, load_env_file, macro_execute, Config, GlobalConfig, Input,
    WorkingMode, TEMP_SESSION_NAME,
};
use crate::hooks::{
    dispatch_hooks_with_count_and_manager, dispatch_hooks_with_managers, drain_async_results,
    inject_pending_async_context, AsyncHookManager, HookEvent, HookResultControl,
    PersistentHookManager,
};
use crate::render::render_error;
use crate::tui::Tui;
use crate::utils::*;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use parking_lot::RwLock;
use simplelog::{format_description, ConfigBuilder, LevelFilter, SimpleLogger, WriteLogger};
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use std::{env, path::PathBuf, sync::Arc, time::Duration};
use tokio::io::{AsyncRead as TokioAsyncRead, AsyncWrite as TokioAsyncWrite, ReadBuf};

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
        WorkingMode::Repl
    } else {
        WorkingMode::Cmd
    };
    let info_flag = cli.info
        || cli.sync_models
        || cli.list_models
        || cli.list_agents
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

    if cli.sync_models {
        let url = config.read().sync_models_url();
        return Config::sync_models(&url, abort_signal.clone()).await;
    }

    if cli.list_models {
        for model in list_models(&config.read(), ModelType::Chat) {
            println!("{}", model.id());
        }
        return Ok(());
    }
    if cli.list_agents {
        let agents = list_agents().join("\n");
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
        let session = cli.session.as_ref().map(|v| match v {
            Some(v) => v.as_str(),
            None => TEMP_SESSION_NAME,
        });
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
        return run_acp_server(config, agent_name).await;
    }
    if let Some(addr) = cli.serve {
        return serve::run(config, addr).await;
    }
    let is_repl = config.read().working_mode.is_repl();
    if cli.rebuild_rag {
        Config::rebuild_rag(&config, abort_signal.clone()).await?;
        if is_repl {
            return Ok(());
        }
    }
    if let Some(name) = &cli.macro_name {
        macro_execute(&config, name, text.as_deref(), abort_signal.clone()).await?;
        return Ok(());
    }
    config.write().apply_default_session()?;
    match is_repl {
        false => {
            let input = create_input(&config, text, &cli.file, abort_signal.clone()).await?;
            let mut async_manager = AsyncHookManager::new();
            let persistent_manager =
                Arc::new(tokio::sync::Mutex::new(PersistentHookManager::new()));
            let mut pending_async_context = None;
            dispatch_session_start(&config, "cmd", &async_manager, &persistent_manager).await;
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
            result
        }
        true => {
            if !*IS_STDOUT_TERMINAL {
                bail!("No TTY for REPL")
            }
            start_interactive(&config).await
        }
    }
}

struct TokioCompat<T> {
    inner: T,
}

impl<T> TokioCompat<T> {
    fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T: TokioAsyncRead + Unpin> futures_util::io::AsyncRead for TokioCompat<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut read_buf = ReadBuf::new(buf);
        match Pin::new(&mut self.inner).poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(read_buf.filled().len())),
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T: TokioAsyncWrite + Unpin> futures_util::io::AsyncWrite for TokioCompat<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

async fn run_acp_server(config: GlobalConfig, agent_name: String) -> Result<()> {
    use tokio::task::LocalSet;

    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("acp-server".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    let _ =
                        result_tx.send(Err(anyhow!("Failed to create ACP server runtime: {err}")));
                    return;
                }
            };

            let local_set = LocalSet::new();
            let result = local_set.block_on(&runtime, async move {
                acp_server_main(config, agent_name).await
            });
            let _ = result_tx.send(result);
        })
        .context("Failed to start ACP server thread")?;

    match result_rx.await {
        Ok(result) => result,
        Err(_) => Err(anyhow!("ACP server thread panicked")),
    }
}

async fn acp_server_main(config: GlobalConfig, agent_name: String) -> Result<()> {
    use crate::acp::HarnxAgent;
    use agent_client_protocol as acp;
    use std::rc::Rc;

    let agent = Rc::new(HarnxAgent::new(agent_name, config));
    let agent_for_conn = Rc::clone(&agent);
    let stdin = tokio::io::stdin();
    #[cfg(unix)]
    let stdout = {
        use std::os::fd::AsFd;

        let owned_fd = std::io::stdout()
            .as_fd()
            .try_clone_to_owned()
            .context("Failed to duplicate stdout fd for ACP server")?;
        tokio::fs::File::from_std(std::fs::File::from(owned_fd))
    };
    #[cfg(not(unix))]
    let stdout = tokio::io::stdout();

    let (conn, io_task) = acp::AgentSideConnection::new(
        agent_for_conn,
        TokioCompat::new(stdout),
        TokioCompat::new(stdin),
        |future| {
            tokio::task::spawn_local(future);
        },
    );

    agent.set_connection(Rc::new(conn));
    io_task
        .await
        .map_err(|err| anyhow!("ACP server I/O error: {err}"))?;
    Ok(())
}

fn hook_dispatch_context(config: &GlobalConfig) -> (crate::hooks::HooksConfig, String, PathBuf) {
    let config = config.read();
    (
        config.resolved_hooks(),
        config
            .session
            .as_ref()
            .map(|session| session.name())
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

async fn exit_session_with_hook(
    config: &GlobalConfig,
    async_manager: &AsyncHookManager,
    persistent_manager: &Arc<tokio::sync::Mutex<PersistentHookManager>>,
) -> Result<()> {
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
        input.use_embeddings(abort_signal.clone()).await?;
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
    let (output, thought, tool_results, usage) =
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
    config.write().after_chat_completion(
        &input,
        &output,
        thought.as_deref(),
        &tool_results,
        &usage,
    )?;
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
            config.write().exit_agent()?;
            crate::config::Config::use_agent(
                config,
                &switch_agent.agent,
                switch_agent.session_id.as_deref(),
                abort_signal.clone(),
            )
            .await?;
            if switch_agent.session_id.is_none() {
                config.write().empty_session()?;
            }
            let new_input = Input::from_str(config, &switch_agent.prompt, None);
            return Box::pin(start_directive_inner(
                config,
                new_input,
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
            let new_input = Input::from_str(config, &context, None);
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
        let new_input = Input::from_str(config, &context, None);
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
    dispatch_session_start(config, "repl", &async_manager, &persistent_manager).await;
    let mut tui: Tui = Tui::init(config, async_manager, persistent_manager.clone())?;
    let result = tui.run().await;
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
        Input::from_str(config, &text.unwrap_or_default(), None)
    } else {
        Input::from_files_with_spinner(
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

fn setup_logger(is_serve: bool) -> Result<()> {
    let (log_level, log_path) = Config::log_config(is_serve)?;
    if log_level == LevelFilter::Off {
        return Ok(());
    }
    let crate_name = env!("CARGO_CRATE_NAME");
    let log_filter = match std::env::var(get_env_name("log_filter")) {
        Ok(v) => v,
        Err(_) => match is_serve {
            true => format!("{crate_name}::serve"),
            false => crate_name.into(),
        },
    };
    let config = ConfigBuilder::new()
        .add_filter_allow(log_filter)
        .set_time_format_custom(format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        ))
        .set_thread_level(LevelFilter::Off)
        .build();
    match log_path {
        None => {
            SimpleLogger::init(log_level, config)?;
        }
        Some(log_path) => {
            ensure_parent_exists(&log_path)?;
            let log_file = std::fs::File::create(log_path)?;
            WriteLogger::init(log_level, config, log_file)?;
        }
    }
    Ok(())
}
