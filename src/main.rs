mod cli;
mod client;
mod config;
mod hooks;
mod mcp;
mod rag;
mod render;
mod repl;
mod serve;
mod tool;
#[macro_use]
mod utils;

#[macro_use]
extern crate log;

use crate::cli::Cli;
use crate::client::{
    call_chat_completions, call_chat_completions_streaming, list_models, ModelType,
};
use crate::config::{
    ensure_parent_exists, list_agents, load_env_file, macro_execute, Config, GlobalConfig, Input,
    WorkingMode, CODE_ROLE, EXPLAIN_SHELL_ROLE, SHELL_ROLE, TEMP_SESSION_NAME,
};
use crate::hooks::{
    dispatch_hooks_with_count_and_manager, dispatch_hooks_with_managers, drain_async_results,
    inject_pending_async_context, AsyncHookManager, HookEvent, HookResultControl,
    PersistentHookManager,
};
use crate::render::render_error;
use crate::repl::Repl;
use crate::utils::*;

use anyhow::{bail, Result};
use clap::Parser;
use inquire::Text;
use parking_lot::RwLock;
use simplelog::{format_description, ConfigBuilder, LevelFilter, SimpleLogger, WriteLogger};
use std::{env, path::PathBuf, process, sync::Arc, time::Duration};

#[tokio::main]
async fn main() -> Result<()> {
    load_env_file()?;
    let cli = Cli::parse();
    let text = cli.text()?;
    let working_mode = if cli.serve.is_some() {
        WorkingMode::Serve
    } else if text.is_none() && cli.file.is_empty() {
        WorkingMode::Repl
    } else {
        WorkingMode::Cmd
    };
    let info_flag = cli.info
        || cli.sync_models
        || cli.list_models
        || cli.list_roles
        || cli.list_agents
        || cli.list_rags
        || cli.list_macros
        || cli.list_sessions;
    setup_logger(working_mode.is_serve())?;
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
    if cli.list_roles {
        let roles = Config::list_roles(true).join("\n");
        println!("{roles}");
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
        } else if let Some(name) = &cli.role {
            config.write().use_role(name)?;
        } else if cli.execute {
            config.write().use_role(SHELL_ROLE)?;
        } else if cli.code {
            config.write().use_role(CODE_ROLE)?;
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
    if cli.execute && !is_repl {
        let input = create_input(&config, text, &cli.file, abort_signal.clone()).await?;
        shell_execute(&config, &SHELL, input, abort_signal.clone()).await?;
        return Ok(());
    }
    config.write().apply_prelude()?;
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
                cli.code,
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
    code_mode: bool,
    abort_signal: AbortSignal,
    async_manager: &mut AsyncHookManager,
    persistent_manager: &Arc<tokio::sync::Mutex<PersistentHookManager>>,
    pending_async_context: &mut Option<String>,
) -> Result<()> {
    start_directive_inner(
        config,
        input,
        code_mode,
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
    code_mode: bool,
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
    let client = input.create_client()?;
    let extract_code = !*IS_STDOUT_TERMINAL && code_mode;
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
    let (output, tool_results) = if !input.stream() || extract_code {
        match call_chat_completions(
            &input,
            true,
            extract_code,
            client.as_ref(),
            abort_signal.clone(),
        )
        .await
        {
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
                return Err(err);
            }
        }
    } else {
        match call_chat_completions_streaming(&input, client.as_ref(), abort_signal.clone()).await {
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
                return Err(err);
            }
        }
    };
    config
        .write()
        .after_chat_completion(&input, &output, &tool_results)?;
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
        return start_directive_inner(
            config,
            input.merge_tool_results(output, tool_results),
            code_mode,
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
                code_mode,
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
            code_mode,
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
    let mut repl: Repl = Repl::init(config, async_manager, persistent_manager.clone())?;
    let result = repl.run().await;
    exit_session_with_hook(config, repl.async_manager(), &persistent_manager).await?;
    persistent_manager.lock().await.shutdown();
    result
}

#[async_recursion::async_recursion]
async fn shell_execute(
    config: &GlobalConfig,
    shell: &Shell,
    mut input: Input,
    abort_signal: AbortSignal,
) -> Result<()> {
    let client = input.create_client()?;
    config.write().before_chat_completion(&input)?;
    let (eval_str, _) =
        call_chat_completions(&input, false, true, client.as_ref(), abort_signal.clone()).await?;

    config
        .write()
        .after_chat_completion(&input, &eval_str, &[])?;
    if eval_str.is_empty() {
        bail!("No command generated");
    }
    if config.read().dry_run {
        config.read().print_markdown(&eval_str)?;
        return Ok(());
    }
    if *IS_STDOUT_TERMINAL {
        let options = ["execute", "revise", "describe", "copy", "quit"];
        let command = color_text(eval_str.trim(), nu_ansi_term::Color::Rgb(255, 165, 0));
        let first_letter_color = nu_ansi_term::Color::Cyan;
        let prompt_text = options
            .iter()
            .map(|v| format!("{}{}", color_text(&v[0..1], first_letter_color), &v[1..]))
            .collect::<Vec<String>>()
            .join(&dimmed_text(" | "));
        loop {
            println!("{command}");
            let answer_char =
                read_single_key(&['e', 'r', 'd', 'c', 'q'], 'e', &format!("{prompt_text}: "))?;

            match answer_char {
                'e' => {
                    debug!("{} {:?}", shell.cmd, &[&shell.arg, &eval_str]);
                    let code = run_command(&shell.cmd, &[&shell.arg, &eval_str], None)?;
                    if code == 0 && config.read().save_shell_history {
                        let _ = append_to_shell_history(&shell.name, &eval_str, code);
                    }
                    process::exit(code);
                }
                'r' => {
                    let revision = Text::new("Enter your revision:").prompt()?;
                    let text = format!("{}\n{revision}", input.text());
                    input.set_text(text);
                    return shell_execute(config, shell, input, abort_signal.clone()).await;
                }
                'd' => {
                    let role = config.read().retrieve_role(EXPLAIN_SHELL_ROLE)?;
                    let input = Input::from_str(config, &eval_str, Some(role));
                    if input.stream() {
                        call_chat_completions_streaming(
                            &input,
                            client.as_ref(),
                            abort_signal.clone(),
                        )
                        .await?;
                    } else {
                        call_chat_completions(
                            &input,
                            true,
                            false,
                            client.as_ref(),
                            abort_signal.clone(),
                        )
                        .await?;
                    }
                    println!();
                    continue;
                }
                'c' => {
                    set_text(&eval_str)?;
                    println!("{}", dimmed_text("✓ Copied the command."));
                }
                _ => {}
            }
            break;
        }
    } else {
        println!("{eval_str}");
    }
    Ok(())
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
