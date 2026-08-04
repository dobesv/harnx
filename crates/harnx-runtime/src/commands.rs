use std::io::Write;

use syntect::highlighting::{Color, Theme};

use crate::config::{
    macro_execute, remote_session_ops, AgentVariables, Config, GlobalConfig, Input, LastMessage,
};
use crate::nats_hook_provider::{
    discover_process_nats_hook_provider, dispatch_hook_event, HookDispatchMeta, HookEventDispatch,
};
use crate::utils::{dimmed_text, set_text, AbortSignal};
use harnx_hooks::{HookEvent, HookResultControl};
use harnx_render::render_error;

use anyhow::{anyhow, bail, Context, Result};
use fancy_regex::Regex;
use std::env;
use std::sync::{Arc, LazyLock};

/// Outcome of running a dot-command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOutcome {
    /// Continue normally.
    Continue,
    /// Exit the interactive session.
    Exit,
    /// Open agent picker in TUI.
    OpenAgentPicker,
    /// Open session picker in TUI.
    OpenSessionPicker,
}

pub static COMMANDS: LazyLock<[Command; 48]> = LazyLock::new(|| {
    [
        Command::new(".help", "Show this help guide"),
        Command::new(".info", "Show system info"),
        Command::new(".info tools", "List all available tools and their status"),
        Command::new(
            ".info env [name]",
            "List harnx process env var names (or show one var's value)",
        ),
        Command::new(".use tool", "Add a tool or toolset to the active tools"),
        Command::new(
            ".drop tool",
            "Remove a tool or toolset from the active tools",
        ),
        Command::new(".edit config", "Modify configuration file"),
        Command::new(".model", "Switch LLM model"),
        Command::new(".prompt", "Set a temporary agent using a prompt"),
        Command::new(".edit agent", "Modify current agent"),
        Command::new(".save agent", "Save current agent to file"),
        Command::new(".session", "Start or switch to a session"),
        Command::new(".empty session", "Clear session messages"),
        Command::new(
            ".reset session",
            "Reset session to initial state (re-expands variables)",
        ),
        Command::new(".reset repl", "Alias for .reset session"),
        Command::new(
            ".compact session",
            "Compact session messages using configured compaction agent",
        ),
        Command::new(".info session", "Show session info"),
        Command::new(
            ".info model",
            "Show active model details (id, client, pricing, vision/tool-use, catalog source)",
        ),
        Command::new(".info theme", "Show active syntax-highlight theme"),
        Command::new(".edit session", "Modify current session"),
        Command::new(
            ".edit message <n>",
            "Edit a single log entry by sequence number",
        ),
        Command::new(
            ".edit message <n>-<m>",
            "Edit a range of log entries by sequence number",
        ),
        Command::new(".save session", "Save current session to file"),
        Command::new(".agent", "Use an agent"),
        Command::new(".starter", "Use a conversation starter"),
        Command::new(".info agent", "Show agent info"),
        Command::new(".rag", "Initialize or access RAG"),
        Command::new(
            ".edit rag-docs",
            "Add or remove documents from an existing RAG",
        ),
        Command::new(".rebuild rag", "Rebuild RAG for document changes"),
        Command::new(".sources rag", "Show citation sources used in last query"),
        Command::new(".info rag", "Show RAG info"),
        Command::new(".exit rag", "Leave RAG"),
        Command::new(".attach", "Attach a file to the next message"),
        Command::new(".detach", "Remove attached files"),
        Command::new(".macro", "Execute a macro"),
        Command::new(".file", "Include files, directories, URLs or commands"),
        Command::new(".continue", "Continue previous response"),
        Command::new(".regenerate", "Regenerate last response"),
        Command::new(".copy", "Copy last response"),
        Command::new(".set", "Modify runtime settings"),
        Command::new(".title", "Show or (re)generate the session title"),
        Command::new(
            ".set show_sequence_numbers",
            "Toggle [n] prefix in transcript (on/off)",
        ),
        Command::new(
            ".set show_timestamps",
            "Toggle timestamp in transcript (on/off)",
        ),
        Command::new(".delete", "Delete agents, sessions, RAGs, or macros"),
        Command::new(
            ".delete message <n>",
            "Delete a log entry by sequence number",
        ),
        Command::new(".delete message <n>-<m>", "Delete a range of log entries"),
        Command::new(
            ".rewind <n>",
            "Rewind session context to entry N (all later entries excluded)",
        ),
        Command::new(".exit", "Exit the interactive session"),
    ]
});
static COMMAND_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(\.\S*)\s*").unwrap());
static MULTILINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^\s*:::\s*(.*)\s*:::\s*$").unwrap());

#[derive(Debug, Clone)]
pub struct Command {
    pub name: &'static str,
    pub description: &'static str,
}

impl Command {
    const fn new(name: &'static str, desc: &'static str) -> Self {
        Self {
            name,
            description: desc,
        }
    }
}

pub async fn run_command(
    config: &GlobalConfig,
    abort_signal: AbortSignal,
    line: &str,
) -> Result<CommandOutcome> {
    let mut stdout_sink = std::io::stdout();
    run_command_with_output(config, abort_signal, line, &mut stdout_sink).await
}

/// Write the harnx process environment. With `name = None`, lists variable
/// names only (no values) — this is the environment hooks and MCP servers
/// inherit, useful for checking e.g. `DBUS_SESSION_BUS_ADDRESS` or whether a
/// token var is present. With a name, prints that variable's value.
fn write_env_info(output: &mut (dyn Write + Send), name: Option<&str>) -> Result<()> {
    match name {
        Some(name) => match std::env::var(name) {
            Ok(value) => writeln!(output, "{name}={value}")?,
            Err(_) => writeln!(output, "{name} is not set")?,
        },
        None => {
            let mut names: Vec<String> = std::env::vars().map(|(key, _)| key).collect();
            names.sort();
            writeln!(
                output,
                "{} environment variables (values hidden; use `.info env <NAME>`):",
                names.len()
            )?;
            for key in names {
                writeln!(output, "  {key}")?;
            }
        }
    }
    Ok(())
}

pub async fn run_command_with_output(
    config: &GlobalConfig,
    abort_signal: AbortSignal,
    line: &str,
    output: &mut (dyn Write + Send),
) -> Result<CommandOutcome> {
    let local_worker = Arc::new(tokio::sync::Mutex::new(None));
    run_command_with_output_and_local_worker(config, abort_signal, line, output, &local_worker)
        .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_command_with_output_and_local_worker(
    config: &GlobalConfig,
    abort_signal: AbortSignal,
    mut line: &str,
    output: &mut (dyn Write + Send),
    local_worker: &Arc<
        tokio::sync::Mutex<Option<crate::local_orchestrator::LocalWorkerSupervisor>>,
    >,
) -> Result<CommandOutcome> {
    if let Ok(Some(captures)) = MULTILINE_RE.captures(line) {
        if let Some(text_match) = captures.get(1) {
            line = text_match.as_str();
        }
    }
    match parse_command(line) {
        Some((cmd, args)) => match cmd {
            ".help" => {
                dump_help(output)?;
            }
            ".title" => match args {
                None => {
                    let conf = config.read();
                    let Some(session) = conf.session.as_ref() else {
                        writeln!(output, "No session")?;
                        return Ok(CommandOutcome::Continue);
                    };
                    let title = session.title().unwrap_or("(none — not generated yet)");
                    // Report the configured name AND how it resolves for the
                    // active agent (which may be a package agent). Resolving
                    // here mirrors what `resolve_title_agent` actually looks up,
                    // so the display matches real behavior (#103).
                    let active_agent = conf.extract_agent();
                    let active_pkg =
                        harnx_core::package_namespace::pkg_from_qualified(active_agent.name());
                    let title_agent = match crate::config::session_ops_title::resolve_title_agent_name(
                        active_agent.title_agent(),
                        conf.title_agent.as_deref(),
                        active_pkg,
                    ) {
                        Some((name, resolved)) if resolved == name => name,
                        Some((name, resolved)) => format!("{name} (resolves to {resolved})"),
                        None => "(not configured)".to_string(),
                    };
                    let last_updated = if session.title_last_updated_tokens() == usize::MAX {
                        "(frozen/manual)".to_string()
                    } else {
                        session.title_last_updated_tokens().to_string()
                    };
                    writeln!(output, "title: {title}")?;
                    writeln!(
                        output,
                        "title_update_threshold: {}",
                        conf.title_update_threshold
                    )?;
                    writeln!(output, "title_agent: {title_agent}")?;
                    writeln!(output, "session.tokens: {}", session.tokens())?;
                    writeln!(output, "title_last_updated_tokens: {last_updated}")?;
                }
                Some("generate" | "now") => {
                    // Generate synchronously so the user sees the outcome right
                    // here. Isolate the title-agent's own model/retry events
                    // from the transcript via NullSink.
                    let result = harnx_core::sink::with_agent_event_sink(
                        Arc::new(harnx_core::event::NullSink),
                        Config::generate_title(config),
                    )
                    .await;
                    // Report the outcome directly as command output. On success
                    // also emit `TitleUpdated` (updates the terminal title and
                    // any UI listeners). Do NOT emit `TitleGenerationFailed` on
                    // error — the error is already shown here, and emitting it
                    // would double-render the failure in the TUI transcript.
                    match result {
                        Ok(Some(title)) => {
                            writeln!(output, "title: {title}")?;
                            harnx_core::sink::emit_agent_event(
                                harnx_core::event::AgentEvent::Session(
                                    harnx_core::event::SessionEvent::TitleUpdated(title),
                                ),
                            );
                        }
                        Ok(None) => writeln!(output, "(nothing to title)")?,
                        Err(err) => writeln!(output, "title generation failed: {err:#}")?,
                    }
                }
                _ => writeln!(output, "Usage: .title [generate|now]")?,
            },
            ".info" => match args {
                Some("session") => {
                    let info = config.read().session_info()?;
                    write!(output, "{info}")?;
                }
                Some("model") => {
                    let conf = config.read();
                    write_model_info_block(output, &conf, conf.current_model())?;
                }
                Some("rag") => {
                    let info = config.read().rag_info()?;
                    write!(output, "{info}")?;
                }
                Some("agent") => {
                    let info = config.read().agent_info()?;
                    write!(output, "{info}")?;
                }
                Some("tools") => {
                    let conf = config.read();
                    // Spell handoff tool names relative to the active agent's
                    // package so they match the active-tool whitelist below and
                    // what the agent actually sees (#709).
                    let active_pkg = conf.active_package();
                    let (declarations, _) =
                        conf.tool_declarations_for_use_tools(Some("*"), active_pkg.as_deref());
                    let active_tools = conf.active_tool_names();
                    if declarations.is_empty() {
                        writeln!(output, "No tools available")?;
                    } else {
                        for decl in &declarations {
                            let marker = if active_tools.contains(&decl.name) {
                                "●"
                            } else {
                                "○"
                            };
                            writeln!(output, "  {} {} - {}", marker, decl.name, decl.description)?;
                        }
                        let active_count = declarations
                            .iter()
                            .filter(|d| active_tools.contains(&d.name))
                            .count();
                        writeln!(
                            output,
                            "\n{} active / {} total",
                            active_count,
                            declarations.len()
                        )?;
                    }
                }
                Some("theme") => {
                    let config = config.read();
                    let mode = if config.light_theme() { "light" } else { "dark" };
                    writeln!(output, "mode: {mode}")?;

                    let render_options = config.render_options()?;
                    if let Some(theme) = render_options.theme.as_ref() {
                        let theme_path = Config::local_path(&format!("{mode}.tmTheme"));
                        let fallback_name = if theme_path.exists() {
                            "(custom theme)"
                        } else if config.light_theme() {
                            "(builtin monokai-extended-light)"
                        } else {
                            "(builtin monokai-extended)"
                        };
                        let theme_name = theme.name.as_deref().unwrap_or(fallback_name);

                        writeln!(output, "theme: {theme_name}")?;
                        if theme_path.exists() {
                            writeln!(output, "source: {}", theme_path.display())?;
                        } else {
                            writeln!(output, "source: builtin")?;
                        }
                        writeln!(
                            output,
                            "foreground: {}",
                            color_to_hex(theme.settings.foreground.as_ref())
                        )?;
                        writeln!(
                            output,
                            "background: {}",
                            color_to_hex(theme.settings.background.as_ref())
                        )?;
                        writeln!(output, "string: {}", scope_color(theme, "string"))?;
                        writeln!(output, "keyword: {}", scope_color(theme, "keyword"))?;
                        writeln!(output, "comment: {}", scope_color(theme, "comment"))?;
                    } else {
                        writeln!(output, "highlighting: disabled")?;
                    }
                }
                Some(rest) if rest == "env" || rest.starts_with("env ") => {
                    let name = rest.strip_prefix("env").map(str::trim).filter(|s| !s.is_empty());
                    write_env_info(output, name)?;
                }
                Some(_) => unknown_command()?,
                None => {
                    let sysinfo = config.read().sysinfo()?;
                    write!(output, "{sysinfo}")?;
                }
            },
            ".model" => match args {
                Some(name) => {
                    let from_model = config.read().current_model().id().to_string();
                    config.write().set_model(name)?;
                    let to_model = config.read().current_model().id().to_string();
                    harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Session(
                        harnx_core::event::SessionEvent::ModelChanged {
                            from: from_model,
                            to: to_model,
                        },
                    ));
                }
                None => writeln!(output, "Usage: .model <name>")?,
            },
            ".prompt" => match args {
                Some(text) => {
                    config.write().use_prompt(text)?;
                }
                None => writeln!(output, "Usage: .prompt <text>...")?,
            },
            ".session" => {
                if args.is_none() {
                    return Ok(CommandOutcome::OpenSessionPicker);
                }
                config.write().use_session(args)?;
            }
            ".rag" => {
                Config::use_rag(config, args, abort_signal.clone()).await?;
            }
            ".agent" => match split_first_arg(args) {
                Some((agent_name, args)) => {
                    let (new_args, _) = split_args_text(args.unwrap_or_default(), cfg!(windows));
                    let (session_name, variable_pairs) = match new_args.first() {
                        Some(name) if name.contains('=') => (None, new_args.as_slice()),
                        Some(name) => (Some(name.as_str()), &new_args[1..]),
                        None => (None, &[] as &[String]),
                    };
                    let variables: AgentVariables = variable_pairs
                        .iter()
                        .filter_map(|v| v.split_once('='))
                        .map(|(key, value)| (key.to_string(), value.to_string()))
                        .collect();
                    if variables.len() != variable_pairs.len() {
                        bail!("Some variable values are not key=value pairs");
                    }
                    if !variables.is_empty() {
                        config.write().agent_variables = Some(variables);
                    }
                    let ret =
                        Config::use_agent(config, agent_name, session_name, abort_signal.clone())
                            .await;
                    config.write().agent_variables = None;
                    ret?;
                }
                None => return Ok(CommandOutcome::OpenAgentPicker),
            },
            ".starter" => match args {
                Some(id) => {
                    let mut text = None;
                    if let Some(agent) = config.read().agent.as_ref() {
                        for (i, value) in agent.conversation_staters().iter().enumerate() {
                            if (i + 1).to_string() == id {
                                text = Some(value.clone());
                            }
                        }
                    }
                    match text {
                        Some(text) => {
                            writeln!(output, "{}", dimmed_text(&format!(">> {text}")))?;
                            let input = crate::config::input::from_str(config, &text, None);
                            ask(
                                config,
                                local_worker,
                                abort_signal.clone(),
                                input,
                                true,
                            )
                            .await?;
                        }
                        None => {
                            bail!("Invalid starter value");
                        }
                    }
                }
                None => {
                    let banner = config.read().agent_banner()?;
                    writeln!(output, "{banner}")?;
                }
            },
            ".save" => match split_first_arg(args) {
                Some(("agent", name)) => {
                    config.write().save_agent(name)?;
                }
                Some(("session", name)) => {
                    config.write().save_session(name)?;
                }
                _ => writeln!(output, r#"Usage: .save <agent|session> [name]"#)?,
            },
            ".edit" => {
                if config.read().macro_flag {
                    bail!("Cannot perform this operation because you are in a macro")
                }
                match args {
                    Some("config") => {
                        config.write().edit_config()?;
                    }
                    Some("agent") => {
                        config.write().edit_agent_prompt()?;
                    }
                    Some("session") => {
                        config.write().edit_session()?;
                    }
                    Some("rag-docs") => {
                        Config::edit_rag_docs(config, abort_signal.clone()).await?;
                    }
                    Some(args) if args.starts_with("message ") => {
                        let (from, to) = parse_message_range(&args[8..])?;
                        remote_session_ops::edit_remote_message_range(
                            config,
                            from,
                            to,
                            &abort_signal,
                        )
                        .await?;
                    }
                    _ => writeln!(output, r#"Usage: .edit <config|agent|session|rag-docs|message <n>|message <n>-<m>>"#)?,
                }
            }
            ".compact" => match args {
                Some("session") => {
                    // Atomically guard against concurrent compaction (auto or
                    // manual) and claim the compacting flag under a single write
                    // lock. The agent loop and auto-compaction both consult
                    // `is_compacting_session()` (the `compressing` flag), so we
                    // must set it here for the duration of the manual run.
                    enum Claim {
                        Claimed,
                        AlreadyCompacting,
                        NoSession,
                    }
                    let claim = {
                        let mut cfg = config.write();
                        match cfg.session.as_mut() {
                            None => Claim::NoSession,
                            Some(session) if session.compressing() => Claim::AlreadyCompacting,
                            Some(session) => {
                                session.set_compressing(true);
                                Claim::Claimed
                            }
                        }
                    };
                    match claim {
                        Claim::NoSession => {
                            writeln!(output, "No active session to compact.")?;
                            return Ok(CommandOutcome::Continue);
                        }
                        Claim::AlreadyCompacting => {
                            writeln!(output, "Compaction already in progress.")?;
                            return Ok(CommandOutcome::Continue);
                        }
                        Claim::Claimed => {}
                    }
                    // Emit start event, run compaction, then emit completion or
                    // failure. Always clear the compacting flag afterwards. All
                    // user-visible feedback flows through the SessionEvents
                    // (rendered by the TUI transcript / CLI spinner sink) so we
                    // do NOT also write to `output` — that would double-render
                    // the message in the TUI/CLI.
                    harnx_core::sink::emit_agent_event(
                        harnx_core::event::AgentEvent::Session(
                            harnx_core::event::SessionEvent::CompactingStarted,
                        ),
                    );
                    let result = Config::compact_session(config).await;
                    if let Some(session) = config.write().session.as_mut() {
                        session.set_compressing(false);
                    }
                    match result {
                        Ok(()) => {
                            harnx_core::sink::emit_agent_event(
                                harnx_core::event::AgentEvent::Session(
                                    harnx_core::event::SessionEvent::CompactingCompleted,
                                ),
                            );
                        }
                        Err(err) => {
                            // Emit the failure event only. Do NOT propagate the
                            // error or write to `output` — either would render
                            // the failure a second time.
                            harnx_core::sink::emit_agent_event(
                                harnx_core::event::AgentEvent::Session(
                                    harnx_core::event::SessionEvent::CompactingFailed(
                                        err.to_string(),
                                    ),
                                ),
                            );
                        }
                    }
                }
                _ => writeln!(output, r#"Usage: .compact session"#)?,
            },
            ".empty" => match args {
                Some("session") => {
                    config.write().empty_session()?;
                }
                _ => writeln!(output, r#"Usage: .empty session"#)?,
            },
            ".reset" => match args {
                Some("session") | Some("repl") => {
                    config.write().reset_session()?;
                }
                _ => {
                    writeln!(output, r#"Usage: .reset session"#)?;
                }
            },
            ".rebuild" => match args {
                Some("rag") => {
                    Config::rebuild_rag(config, abort_signal.clone()).await?;
                }
                _ => writeln!(output, r#"Usage: .rebuild rag"#)?,
            },
            ".sources" => match args {
                Some("rag") => {
                    let sources = Config::rag_sources(config)?;
                    writeln!(output, "{sources}")?;
                }
                _ => writeln!(output, r#"Usage: .sources rag"#)?,
            },
            ".macro" => match split_first_arg(args) {
                Some((name, extra)) => {
                    if !Config::has_macro(name) && extra.is_none() {
                        config.write().new_macro(name)?;
                    } else {
                        macro_execute(config, name, extra, abort_signal.clone()).await?;
                    }
                }
                None => writeln!(output, "Usage: .macro <name> <text>...")?,
            },
            ".file" => match args {
                Some(args) => {
                    let (files, text) = split_args_text(args, cfg!(windows));
                    let input = crate::config::input::from_files_with_spinner(
                        config,
                        text,
                        files,
                        None,
                        abort_signal.clone(),
                    )
                    .await?;
                    ask(
                        config,
                        local_worker,
                        abort_signal.clone(),
                        input,
                        true,
                    )
                    .await?;
                }
                None => crate::utils::emit_info(
                    r#"Usage: .file <file|dir|url|cmd|loader:resource|%%>... [-- <text>...]

.file /tmp/file.txt
.file src/ Cargo.toml -- analyze
.file https://example.com/file.txt -- summarize
.file https://example.com/image.png -- recognize text
.file `git diff` -- Generate git commit message
.file jina:https://example.com
.file %% -- translate last reply to english"#
                        .to_string(),
                ),
            },
            ".continue" => {
                let LastMessage {
                    mut input, output, ..
                } = match config
                    .read()
                    .last_message
                    .as_ref()
                    .filter(|v| v.continuous && !v.output.is_empty())
                    .cloned()
                {
                    Some(v) => v,
                    None => bail!("Unable to continue the response"),
                };
                input.set_continue_output(&output);
                ask(
                    config,
                    local_worker,
                    abort_signal.clone(),
                    input,
                    true,
                )
                .await?;
            }
            ".regenerate" => {
                let LastMessage { mut input, .. } = match config
                    .read()
                    .last_message
                    .as_ref()
                    .filter(|v| v.continuous)
                    .cloned()
                {
                    Some(v) => v,
                    None => bail!("Unable to regenerate the response"),
                };
                crate::config::input::set_regenerate(&mut input, config);
                ask(
                    config,
                    local_worker,
                    abort_signal.clone(),
                    input,
                    true,
                )
                .await?;
            }
            ".use" => match split_first_arg(args) {
                Some(("tool", name)) => {
                    let name = name.map(|n| n.trim()).unwrap_or("");
                    if name.is_empty() {
                        writeln!(
                            output,
                            "Usage: .use tool <name>  (tool name, toolset name, or <server>_*)"
                        )?;
                    } else {
                        let mut conf = config.write();
                        let current = conf.extract_agent().use_tools().unwrap_or_default();
                        if current.iter().any(|v| v == name) {
                            writeln!(output, "'{}' is already in use_tools", name)?;
                        } else {
                            let mut new_items = current;
                            new_items.push(name.to_string());
                            conf.set_use_tools(Some(new_items));
                            writeln!(output, "Added '{}' to use_tools", name)?;
                        }
                    }
                }
                _ => writeln!(output, "Usage: .use tool <name>")?,
            },
            ".drop" => match split_first_arg(args) {
                Some(("tool", name)) => {
                    let name = name.map(|n| n.trim()).unwrap_or("");
                    if name.is_empty() {
                        writeln!(output, "Usage: .drop tool <name>")?;
                    } else {
                        let mut conf = config.write();
                        let current = conf.extract_agent().use_tools().unwrap_or_default();
                        if !current.iter().any(|v| v == name) {
                            writeln!(output, "'{}' is not in use_tools", name)?;
                        } else {
                            let remaining: Vec<String> =
                                current.into_iter().filter(|i| i != name).collect();
                            let new_value = if remaining.is_empty() {
                                None
                            } else {
                                Some(remaining)
                            };
                            conf.set_use_tools(new_value);
                            writeln!(output, "Removed '{}' from use_tools", name)?;
                        }
                    }
                }
                _ => writeln!(output, "Usage: .drop tool <name>")?,
            },
            ".set" => match args {
                Some(args) => {
                    Config::update(config, args)?;
                }
                _ => writeln!(output, "Usage: .set <key> <value>...")?,
            },
            ".delete" => match args {
                Some(args) if args.starts_with("message ") => {
                    let (from, to) = parse_message_range(&args[8..])?;
                    remote_session_ops::delete_remote_message_range(
                        config,
                        from,
                        to,
                        &abort_signal,
                    )
                    .await?;
                    writeln!(output, "Deleted entries {from}-{to}.")?;
                }
                Some(args) => {
                    Config::delete(config, args)?;
                }
                _ => writeln!(
                    output,
                    "Usage: .delete <agent|session|rag|macro|agent-data|message <n>|message <n>-<m>>"
                )?,
            },
            ".rewind" => {
                let n = args
                    .ok_or_else(|| anyhow!("Usage: .rewind <n>"))?
                    .trim()
                    .parse::<usize>()
                    .context("Invalid sequence number")?;
                remote_session_ops::rewind_remote_session(config, n, &abort_signal).await?;
                writeln!(output, "↩ Rewound session to entry {n}.")?;
            }
            ".copy" => {
                let output = match config
                    .read()
                    .last_message
                    .as_ref()
                    .filter(|v| !v.output.is_empty())
                    .map(|v| v.output.clone())
                {
                    Some(v) => v,
                    None => bail!("No chat response to copy"),
                };
                set_text(&output).context("Failed to copy the last chat response")?;
            }
            ".exit" => match args {
                Some("rag") => {
                    config.write().exit_rag()?;
                }
                Some(_) => unknown_command()?,
                None => {
                    return Ok(CommandOutcome::Exit);
                }
            },
            ".clear" => match args {
                Some("messages") => {
                    bail!("Use '.empty session' instead");
                }
                _ => unknown_command()?,
            },
            _ => unknown_command()?,
        },
        None => {
            let (session_id, config_snapshot) = {
                let config = config.read();
                (
                    config
                        .session
                        .as_ref()
                        .map(|session| session.id())
                        .unwrap_or("default")
                        .to_string(),
                    config.clone(),
                )
            };
            let nats_hook_provider =
                discover_process_nats_hook_provider(&config_snapshot).await;
            let cwd = env::current_dir().unwrap_or_default();
            let event = HookEvent::UserPromptSubmit {
                prompt: line.to_string(),
            };
            let outcome = dispatch_hook_event(
                HookEventDispatch {
                    event,
                    provider: nats_hook_provider.as_deref(),
                    meta: HookDispatchMeta {
                        session_id: session_id.clone(),
                        cwd: cwd.clone(),
                        resume_count: 0,
                    },
                    pending_async_context: None,
                },
            )
            .await;
            match outcome.control {
                HookResultControl::Block { reason } => {
                    render_error(anyhow!(reason));
                }
                HookResultControl::Ask { .. } => {
                    // Ask is not applicable for UserPromptSubmit event, treat as Continue
                }
                HookResultControl::Continue => {
                    let input_text = match outcome.result.additional_context {
                        Some(additional_context) if !additional_context.is_empty() => {
                            format!("{line}\n\n{additional_context}")
                        }
                        _ => line.to_string(),
                    };
                    let input = crate::config::input::from_str(config, &input_text, None);
                    ask(
                        config,
                        local_worker,
                        abort_signal.clone(),
                        input,
                        true,
                    )
                    .await?;
                }
            }
        }
    }

    Ok(CommandOutcome::Continue)
}

#[allow(clippy::too_many_arguments)]
async fn ask(
    config: &GlobalConfig,
    local_worker: &Arc<
        tokio::sync::Mutex<Option<crate::local_orchestrator::LocalWorkerSupervisor>>,
    >,
    abort_signal: AbortSignal,
    mut input: Input,
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
                crate::config::LOCAL_CLUSTER_KEY.to_string(),
            )
        });
        let session_id = cfg.session.as_ref().map(|session| session.id().to_string());
        (agent, cluster, session_id)
    };

    if cluster == crate::config::LOCAL_CLUSTER_KEY {
        let mut supervisor = local_worker.lock().await;
        crate::local_orchestrator::ensure_local_worker(&mut supervisor)
            .await
            .context("failed to ensure local NATS worker")?;
    }

    let session = crate::ThinClientSession::from_global_config(
        crate::ThinClientConfig {
            cluster,
            agent,
            session_id,
        },
        config,
        abort_signal,
    )
    .await
    .context("failed to create thin-client session for command")?;
    let sink = harnx_core::sink::current_agent_event_sink()
        .unwrap_or_else(|| Arc::new(harnx_core::event::NullSink));
    let result = session.run_turn(&input.text(), sink, None).await?;
    update_last_message_after_thin_client_turn(config, input, &result);
    Ok(())
}

/// Update front-end continuation state after a completed thin-client turn.
pub fn update_last_message_after_thin_client_turn(
    config: &GlobalConfig,
    input: Input,
    result: &crate::ThinClientTurnResult,
) {
    if result.was_cancelled {
        return;
    }
    if let Some(response) = &result.response {
        config.write().last_message = Some(LastMessage::new(input, response.clone()));
    }
}

fn unknown_command() -> Result<()> {
    bail!(r#"Unknown command. Type ".help" for additional help."#);
}

fn dump_help(output: &mut (dyn Write + Send)) -> Result<()> {
    let head = COMMANDS
        .iter()
        .map(|cmd| format!("{:<24} {}", cmd.name, cmd.description))
        .collect::<Vec<String>>()
        .join("\n");
    writeln!(
        output,
        r###"{head}

Type ::: to start multi-line editing, type ::: to finish it.
Press Ctrl+C to cancel the response, Ctrl+D to exit."###,
    )?;
    Ok(())
}

fn parse_message_range(s: &str) -> Result<(usize, usize)> {
    let s = s.trim();
    if let Some((from, to)) = s.split_once('-') {
        let from = from
            .trim()
            .parse::<usize>()
            .context("Invalid starting sequence number")?;
        let to = to
            .trim()
            .parse::<usize>()
            .context("Invalid ending sequence number")?;
        if from > to {
            bail!("Invalid range: start is greater than end");
        }
        Ok((from, to))
    } else {
        let n = s.parse::<usize>().context("Invalid sequence number")?;
        Ok((n, n))
    }
}

fn parse_command(line: &str) -> Option<(&str, Option<&str>)> {
    match COMMAND_RE.captures(line) {
        Ok(Some(captures)) => {
            let cmd = captures.get(1)?.as_str();
            let args = line[captures[0].len()..].trim();
            let args = if args.is_empty() { None } else { Some(args) };
            Some((cmd, args))
        }
        _ => None,
    }
}

fn split_first_arg(args: Option<&str>) -> Option<(&str, Option<&str>)> {
    args.map(|v| match v.split_once(' ') {
        Some((subcmd, args)) => (subcmd, Some(args.trim())),
        None => (v, None),
    })
}

fn model_source_label(conf: &Config, model: &crate::client::Model) -> &'static str {
    if crate::client::list_all_models(&conf.clients)
        .iter()
        .any(|candidate| {
            candidate.id() == model.id() && candidate.model_type() == model.model_type()
        })
    {
        "catalog"
    } else {
        "fallback/default (not found in catalog — capabilities may be wrong!)"
    }
}

fn format_option<T: std::fmt::Display>(value: Option<T>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "—".to_string())
}

fn write_model_info_block(
    output: &mut (dyn Write + Send),
    conf: &Config,
    model: &crate::client::Model,
) -> Result<()> {
    writeln!(output, "model: {}", model.id())?;
    writeln!(output, "client: {}", model.client_name())?;
    writeln!(output, "real_name: {}", model.real_name())?;
    writeln!(output, "type: {}", model.model_type())?;
    writeln!(output, "source: {}", model_source_label(conf, model))?;
    writeln!(output, "supports_vision: {}", model.supports_vision())?;
    writeln!(output, "supports_tool_use: {}", model.supports_tool_use())?;
    writeln!(
        output,
        "max_input_tokens: {}",
        format_option(model.max_input_tokens())
    )?;
    writeln!(
        output,
        "max_output_tokens: {}",
        format_option(model.max_output_tokens())
    )?;
    writeln!(
        output,
        "input_price: {}",
        format_option(model.input_price())
    )?;
    writeln!(
        output,
        "output_price: {}",
        format_option(model.output_price())
    )?;
    Ok(())
}

fn color_to_hex(color: Option<&Color>) -> String {
    match color {
        Some(Color { r, g, b, .. }) => format!("#{r:02X}{g:02X}{b:02X}"),
        None => "none".to_string(),
    }
}

fn scope_color(theme: &Theme, scope_name: &str) -> String {
    theme
        .scopes
        .iter()
        .find(|item| {
            item.scope.selectors.iter().any(|selector| {
                selector
                    .path
                    .scopes
                    .iter()
                    .any(|scope| scope.to_string() == scope_name)
            })
        })
        .and_then(|item| item.style.foreground.as_ref())
        .map_or_else(|| "default".to_string(), |color| color_to_hex(Some(color)))
}

pub fn split_args_text(line: &str, is_win: bool) -> (Vec<String>, &str) {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut unbalance: Option<char> = None;
    let mut prev_char: Option<char> = None;
    let mut text_starts_at = None;
    let unquote_word = |word: &str| {
        if ((word.starts_with('"') && word.ends_with('"'))
            || (word.starts_with('\'') && word.ends_with('\'')))
            && word.len() >= 2
        {
            word[1..word.len() - 1].to_string()
        } else {
            word.to_string()
        }
    };
    let chars: Vec<char> = line.chars().collect();

    for (i, char) in chars.iter().cloned().enumerate() {
        match unbalance {
            Some(ub_char) if ub_char == char => {
                word.push(char);
                unbalance = None;
            }
            Some(_) => {
                word.push(char);
            }
            None => match char {
                ' ' | '\t' | '\r' | '\n' => {
                    if char == '\r' && chars.get(i + 1) == Some(&'\n') {
                        continue;
                    }
                    if let Some('\\') = prev_char.filter(|_| !is_win) {
                        word.push(char);
                    } else if !word.is_empty() {
                        if word == "--" {
                            word.clear();
                            text_starts_at = Some(i + 1);
                            break;
                        }
                        words.push(unquote_word(&word));
                        word.clear();
                    }
                }
                '\'' | '"' | '`' => {
                    word.push(char);
                    unbalance = Some(char);
                }
                '\\' => {
                    if is_win || prev_char.map(|c| c == '\\').unwrap_or_default() {
                        word.push(char);
                    }
                }
                _ => {
                    word.push(char);
                }
            },
        }
        prev_char = Some(char);
    }

    if !word.is_empty() && word != "--" {
        words.push(unquote_word(&word));
    }
    let text = match text_starts_at {
        Some(start) => &line[start..],
        None => "",
    };

    (words, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, WorkingMode};
    use harnx_core::config_data::ConfigData;
    use parking_lot::RwLock;
    use std::path::Path;
    use std::sync::Arc;

    struct EnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn new(key: &'static str, value: &std::path::Path) -> Self {
            let prev = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn thin_client_result_updates_last_message_only_after_completed_turn() {
        let config = Arc::new(RwLock::new(Config::default()));
        let input = crate::config::input::from_str(&config, "hello", None);
        let completed = crate::ThinClientTurnResult {
            response: Some("world".to_string()),
            session_id: "session".to_string(),
            was_cancelled: false,
            user_msg_seq: 1,
            user_msg_id: "message".to_string(),
        };

        update_last_message_after_thin_client_turn(&config, input.clone(), &completed);
        let last = config
            .read()
            .last_message
            .clone()
            .expect("last message set");
        assert_eq!(last.input.text(), "hello");
        assert_eq!(last.output, "world");
        assert!(last.continuous);

        let cancelled = crate::ThinClientTurnResult {
            response: Some("partial".to_string()),
            was_cancelled: true,
            ..completed
        };
        update_last_message_after_thin_client_turn(&config, input, &cancelled);
        assert_eq!(
            config
                .read()
                .last_message
                .as_ref()
                .map(|last| last.output.as_str()),
            Some("world")
        );
    }

    fn write_text(path: &Path, text: &str) {
        std::fs::write(path, text).expect("write test file");
    }

    fn test_config_with_model(model: crate::client::Model) -> GlobalConfig {
        let mut config = Config {
            model,
            working_mode: WorkingMode::Cmd,
            ..Default::default()
        };
        config.session = Some(config::session::new(&config, "test", None).expect("test session"));
        Arc::new(RwLock::new(config))
    }

    #[test]
    fn test_info_env_lists_names_without_values_and_shows_one() {
        let key = "HARNX_TEST_ENV_PROBE";
        let _guard = EnvGuard::new(key, Path::new("secret-value-123"));

        let mut out = Vec::new();
        write_env_info(&mut out, None).expect("list env");
        let listing = String::from_utf8(out).expect("utf8");
        assert!(listing.contains(key), "name missing: {listing}");
        assert!(
            !listing.contains("secret-value-123"),
            "value leaked in listing: {listing}"
        );

        let mut out = Vec::new();
        write_env_info(&mut out, Some(key)).expect("show env");
        assert_eq!(
            String::from_utf8(out).unwrap().trim(),
            "HARNX_TEST_ENV_PROBE=secret-value-123"
        );

        let mut out = Vec::new();
        write_env_info(&mut out, Some("HARNX_DEFINITELY_UNSET_XYZ")).expect("show unset");
        assert!(String::from_utf8(out).unwrap().contains("is not set"));
    }

    #[tokio::test]
    async fn title_command_shows_current_title_and_guard_state() {
        let mut config = Config {
            data: ConfigData {
                title_agent: Some("title-agent".to_string()),
                title_update_threshold: 12_345,
                ..Default::default()
            },
            working_mode: WorkingMode::Cmd,
            ..Default::default()
        };
        let mut session = config::session::new(&config, "test", None).expect("test session");
        session.set_title("Inspect title generation".to_string());
        session.set_title_last_updated_tokens(42);
        config.session = Some(session);
        let config = Arc::new(RwLock::new(config));
        let mut output = Vec::new();
        let abort_signal = crate::utils::create_abort_signal();

        let outcome = run_command_with_output(&config, abort_signal, ".title", &mut output)
            .await
            .expect("command succeeds");

        assert_eq!(outcome, CommandOutcome::Continue);
        assert_eq!(
            String::from_utf8(output).expect("utf8 output"),
            "title: Inspect title generation\ntitle_update_threshold: 12345\ntitle_agent: title-agent\nsession.tokens: 0\ntitle_last_updated_tokens: 42\n"
        );
    }

    #[tokio::test]
    async fn title_command_shows_frozen_manual_without_sentinel_value() {
        // A manually set title freezes regeneration by setting
        // title_last_updated_tokens to usize::MAX; the display must show only
        // "(frozen/manual)", not the raw sentinel integer.
        let mut config = Config {
            data: ConfigData {
                title_agent: Some("title-agent".to_string()),
                title_update_threshold: 12_345,
                ..Default::default()
            },
            working_mode: WorkingMode::Cmd,
            ..Default::default()
        };
        let mut session = config::session::new(&config, "test", None).expect("test session");
        session.set_title("Manual title".to_string());
        session.set_title_last_updated_tokens(usize::MAX);
        config.session = Some(session);
        let config = Arc::new(RwLock::new(config));
        let mut output = Vec::new();
        let abort_signal = crate::utils::create_abort_signal();

        run_command_with_output(&config, abort_signal, ".title", &mut output)
            .await
            .expect("command succeeds");

        let out = String::from_utf8(output).expect("utf8 output");
        assert!(
            out.contains("title_last_updated_tokens: (frozen/manual)\n"),
            "expected frozen/manual annotation without sentinel, got: {out:?}"
        );
        assert!(
            !out.contains(&usize::MAX.to_string()),
            "raw usize::MAX sentinel must not appear, got: {out:?}"
        );
    }

    #[tokio::test]
    async fn title_generate_without_a_loadable_agent_reports_failure() {
        // Hermetic: `title_agent` names an agent that isn't present on disk, so
        // `generate_title` fails at agent resolution. Exercises the command's
        // `Err` branch deterministically (no LLM/network) and asserts the error
        // is surfaced directly as command output (not double-rendered).
        let mut config = Config {
            data: ConfigData {
                title_agent: Some("definitely-missing-title-agent".to_string()),
                ..Default::default()
            },
            working_mode: WorkingMode::Cmd,
            ..Default::default()
        };
        let session = config::session::new(&config, "test", None).expect("test session");
        config.session = Some(session);
        let config = Arc::new(RwLock::new(config));
        let mut output = Vec::new();
        let abort_signal = crate::utils::create_abort_signal();

        let outcome =
            run_command_with_output(&config, abort_signal, ".title generate", &mut output)
                .await
                .expect("command succeeds");

        assert_eq!(outcome, CommandOutcome::Continue);
        let out = String::from_utf8(output).expect("utf8 output");
        assert!(
            out.starts_with("title generation failed:"),
            "expected a failure line, got: {out:?}"
        );
    }

    #[tokio::test]
    async fn title_command_rejects_unknown_subcommand() {
        let config = Config {
            working_mode: WorkingMode::Cmd,
            ..Default::default()
        };
        let config = Arc::new(RwLock::new(config));
        let mut output = Vec::new();
        let abort_signal = crate::utils::create_abort_signal();

        run_command_with_output(&config, abort_signal, ".title bogus", &mut output)
            .await
            .expect("command succeeds");

        assert_eq!(
            String::from_utf8(output).expect("utf8 output"),
            "Usage: .title [generate|now]\n"
        );
    }

    fn model_with_data(
        client_name: &str,
        name: &str,
        supports_vision: bool,
    ) -> crate::client::Model {
        let mut model = crate::client::Model::new(client_name, name);
        model.data_mut().supports_vision = supports_vision;
        model
    }

    #[tokio::test]
    async fn test_info_model_reports_catalog_model() {
        let mut openai_client = crate::client::ClientConfig::OpenAIConfig(Default::default());
        openai_client.set_name("openai".to_string());
        let mut config = Config {
            clients: vec![openai_client],
            model: model_with_data("openai", "gpt-4o", true),
            working_mode: WorkingMode::Cmd,
            ..Default::default()
        };
        config.session = Some(config::session::new(&config, "test", None).expect("test session"));
        let config = Arc::new(RwLock::new(config));
        let mut output = Vec::new();
        let abort_signal = crate::utils::create_abort_signal();

        let outcome = run_command_with_output(&config, abort_signal, ".info model", &mut output)
            .await
            .expect("command succeeds");

        assert_eq!(outcome, CommandOutcome::Continue);
        let output = String::from_utf8(output).expect("utf8 output");
        assert!(output.contains("model: openai:gpt-4o"));
        assert!(output.contains("supports_vision: true"));
        assert!(output.contains("source: catalog"));
    }

    #[tokio::test]
    async fn test_info_model_reports_fallback_model() {
        let config =
            test_config_with_model(model_with_data("test-client", "fallback-model", false));
        let mut output = Vec::new();
        let abort_signal = crate::utils::create_abort_signal();

        run_command_with_output(&config, abort_signal, ".info model", &mut output)
            .await
            .expect("command succeeds");

        let output = String::from_utf8(output).expect("utf8 output");
        assert!(output.contains("model: test-client:fallback-model"));
        assert!(output.contains("supports_vision: false"));
        assert!(output.contains("source: fallback/default"));
    }

    fn test_config_with_theme(theme: &str, highlight: bool) -> GlobalConfig {
        let mut config = Config {
            data: ConfigData {
                theme: Some(theme.to_string()),
                highlight,
                ..Default::default()
            },
            working_mode: WorkingMode::Cmd,
            ..Default::default()
        };
        config.session = Some(config::session::new(&config, "test", None).expect("test session"));
        Arc::new(RwLock::new(config))
    }

    async fn run_info_theme(config: &GlobalConfig) -> String {
        let mut output = Vec::new();
        let abort_signal = crate::utils::create_abort_signal();

        let outcome = run_command_with_output(config, abort_signal, ".info theme", &mut output)
            .await
            .expect("command succeeds");

        assert_eq!(outcome, CommandOutcome::Continue);
        String::from_utf8(output).expect("utf8 output")
    }

    #[tokio::test]
    async fn info_theme_reports_builtin_dark_and_light_modes() {
        let dark_config = test_config_with_theme("dark", true);
        let dark_output = run_info_theme(&dark_config).await;
        assert!(dark_output.contains("mode: dark"));
        assert!(dark_output.contains("theme: Monokai Extended"));
        assert!(dark_output.contains("source: builtin"));
        assert!(dark_output.contains("background: #"));

        let light_config = test_config_with_theme("light", true);
        let light_output = run_info_theme(&light_config).await;
        assert!(light_output.contains("mode: light"));
        assert!(light_output.contains("theme: Monokai Extended Light"));
        assert!(light_output.contains("source: builtin"));
        assert!(light_output.contains("background: #"));
    }

    #[tokio::test]
    async fn info_theme_reports_disabled_highlighting() {
        let config = test_config_with_theme("dark", false);
        let output = run_info_theme(&config).await;

        assert!(output.contains("mode: dark"));
        assert!(output.contains("highlighting: disabled"));
        assert!(!output.contains("theme:"));
        assert!(!output.contains("source:"));
    }

    #[tokio::test]
    async fn info_theme_reports_custom_theme_path_and_name() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());
        write_text(
            &temp.path().join("dark.tmTheme"),
            r##"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>name</key>
  <string>Dracula</string>
  <key>settings</key>
  <array>
    <dict>
      <key>settings</key>
      <dict>
        <key>foreground</key>
        <string>#F8F8F2</string>
        <key>background</key>
        <string>#282A36</string>
      </dict>
    </dict>
    <dict>
      <key>scope</key>
      <string>string</string>
      <key>settings</key>
      <dict>
        <key>foreground</key>
        <string>#F1FA8C</string>
      </dict>
    </dict>
    <dict>
      <key>scope</key>
      <string>keyword</string>
      <key>settings</key>
      <dict>
        <key>foreground</key>
        <string>#FF79C6</string>
      </dict>
    </dict>
    <dict>
      <key>scope</key>
      <string>comment</string>
      <key>settings</key>
      <dict>
        <key>foreground</key>
        <string>#6272A4</string>
      </dict>
    </dict>
  </array>
</dict>
</plist>
"##,
        );

        let config = test_config_with_theme("dark", true);
        let output = run_info_theme(&config).await;

        assert!(output.contains("mode: dark"));
        assert!(output.contains("theme: Dracula"));
        assert!(output.contains(&format!(
            "source: {}",
            temp.path().join("dark.tmTheme").display()
        )));
    }

    /// Tests that a custom .tmTheme without a `<name>` key falls back to "(custom theme)".
    /// syntect's ThemeSet::get_theme parses themes without a name key, resulting in
    /// `theme.name == None`, which triggers the fallback in the `.info theme` output.
    #[tokio::test]
    async fn info_theme_reports_custom_theme_without_name() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());
        // Minimal valid .tmTheme plist WITHOUT a <key>name</key> entry.
        // Contains only the required settings array with global foreground/background.
        write_text(
            &temp.path().join("dark.tmTheme"),
            r##"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>settings</key>
  <array>
    <dict>
      <key>settings</key>
      <dict>
        <key>foreground</key>
        <string>#F8F8F2</string>
        <key>background</key>
        <string>#282A36</string>
      </dict>
    </dict>
    <dict>
      <key>scope</key>
      <string>string</string>
      <key>settings</key>
      <dict>
        <key>foreground</key>
        <string>#F1FA8C</string>
      </dict>
    </dict>
    <dict>
      <key>scope</key>
      <string>keyword</string>
      <key>settings</key>
      <dict>
        <key>foreground</key>
        <string>#FF79C6</string>
      </dict>
    </dict>
    <dict>
      <key>scope</key>
      <string>comment</string>
      <key>settings</key>
      <dict>
        <key>foreground</key>
        <string>#6272A4</string>
      </dict>
    </dict>
  </array>
</dict>
</plist>
"##,
        );

        let config = test_config_with_theme("dark", true);
        let output = run_info_theme(&config).await;

        assert!(output.contains("mode: dark"));
        assert!(output.contains("theme: (custom theme)"));
        assert!(output.contains(&format!(
            "source: {}",
            temp.path().join("dark.tmTheme").display()
        )));
        // Should NOT report builtin
        assert!(!output.contains("source: builtin"));
    }

    #[test]
    fn test_process_command_line() {
        assert_eq!(parse_command(" ."), Some((".", None)));
        assert_eq!(parse_command(" .agent"), Some((".agent", None)));
        assert_eq!(parse_command(" .agent  "), Some((".agent", None)));
        assert_eq!(
            parse_command(" .set dry_run true"),
            Some((".set", Some("dry_run true")))
        );
        assert_eq!(
            parse_command(" .set dry_run true  "),
            Some((".set", Some("dry_run true")))
        );
        assert_eq!(
            parse_command(".prompt \nabc\n"),
            Some((".prompt", Some("abc")))
        );
    }

    #[test]
    fn test_parse_message_range() {
        assert_eq!(parse_message_range("5").unwrap(), (5, 5));
        assert_eq!(parse_message_range("3-7").unwrap(), (3, 7));
        assert_eq!(parse_message_range(" 9 - 12 ").unwrap(), (9, 12));
        assert!(parse_message_range("").is_err());
        assert!(parse_message_range("abc").is_err());
        assert!(parse_message_range("7-3").is_err());
        assert!(parse_message_range("1-2-3").is_err());
    }

    #[test]
    fn test_split_args_text() {
        assert_eq!(split_args_text("", false), (vec![], ""));
        assert_eq!(
            split_args_text("file.txt", false),
            (vec!["file.txt".into()], "")
        );
        assert_eq!(
            split_args_text("file.txt --", false),
            (vec!["file.txt".into()], "")
        );
        assert_eq!(
            split_args_text("file.txt -- hello", false),
            (vec!["file.txt".into()], "hello")
        );
        assert_eq!(
            split_args_text("file.txt -- \thello", false),
            (vec!["file.txt".into()], "\thello")
        );
        assert_eq!(
            split_args_text("file.txt --\nhello", false),
            (vec!["file.txt".into()], "hello")
        );
        assert_eq!(
            split_args_text("file.txt --\r\nhello", false),
            (vec!["file.txt".into()], "hello")
        );
        assert_eq!(
            split_args_text("file.txt --\rhello", false),
            (vec!["file.txt".into()], "hello")
        );
        assert_eq!(
            split_args_text(r#"file1.txt 'file2.txt' "file3.txt""#, false),
            (
                vec!["file1.txt".into(), "file2.txt".into(), "file3.txt".into()],
                ""
            )
        );
        assert_eq!(
            split_args_text(r#"./file1.txt 'file1 - Copy.txt' file\ 2.txt"#, false),
            (
                vec![
                    "./file1.txt".into(),
                    "file1 - Copy.txt".into(),
                    "file 2.txt".into()
                ],
                ""
            )
        );
        assert_eq!(
            split_args_text(r#".\file.txt C:\dir\file.txt"#, true),
            (vec![".\\file.txt".into(), "C:\\dir\\file.txt".into()], "")
        );
    }
}
