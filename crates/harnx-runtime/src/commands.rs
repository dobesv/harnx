use std::io::Write;

use crate::config::{macro_execute, AgentVariables, Config, GlobalConfig, Input, LastMessage};
use crate::utils::{abortable_run_with_spinner, dimmed_text, set_text, AbortSignal};
use harnx_hooks::{
    dispatch_hooks_with_managers, AsyncHookManager, HookEvent, HookResultControl,
    PersistentHookManager,
};
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
}

pub static COMMANDS: LazyLock<[Command; 40]> = LazyLock::new(|| {
    [
        Command::new(".help", "Show this help guide"),
        Command::new(".info", "Show system info"),
        Command::new(".info tools", "List all available tools and their status"),
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
        Command::new(".edit session", "Modify current session"),
        Command::new(".save session", "Save current session to file"),
        Command::new(".exit session", "Exit active session"),
        Command::new(".agent", "Use an agent"),
        Command::new(".starter", "Use a conversation starter"),
        Command::new(".info agent", "Show agent info"),
        Command::new(".exit agent", "Leave agent"),
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
        Command::new(".mcp", "Manage MCP servers"),
        Command::new(".file", "Include files, directories, URLs or commands"),
        Command::new(".continue", "Continue previous response"),
        Command::new(".regenerate", "Regenerate last response"),
        Command::new(".copy", "Copy last response"),
        Command::new(".set", "Modify runtime settings"),
        Command::new(".delete", "Delete agents, sessions, RAGs, or macros"),
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
    async_manager: &mut AsyncHookManager,
    persistent_manager: &std::sync::Arc<tokio::sync::Mutex<PersistentHookManager>>,
    pending_async_context: &mut Option<String>,
) -> Result<CommandOutcome> {
    let mut stdout_sink = std::io::stdout();
    run_command_with_output(
        config,
        abort_signal,
        line,
        async_manager,
        persistent_manager,
        pending_async_context,
        &mut stdout_sink,
    )
    .await
}

pub async fn run_command_with_output(
    config: &GlobalConfig,
    abort_signal: AbortSignal,
    mut line: &str,
    async_manager: &mut AsyncHookManager,
    persistent_manager: &std::sync::Arc<tokio::sync::Mutex<PersistentHookManager>>,
    pending_async_context: &mut Option<String>,
    output: &mut (dyn Write + Send),
) -> Result<CommandOutcome> {
    let max_resume = config.read().resolved_hooks().max_resume.unwrap_or(5);
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
            ".info" => match args {
                Some("session") => {
                    let info = config.read().session_info()?;
                    write!(output, "{info}")?;
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
                    let declarations = conf.tool_declarations_for_use_tools(Some("*"));
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
                Some(_) => unknown_command()?,
                None => {
                    let sysinfo = config.read().sysinfo()?;
                    write!(output, "{sysinfo}")?;
                }
            },
            ".model" => match args {
                Some(name) => {
                    config.write().set_model(name)?;
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
                config.write().use_session(args)?;
                Config::maybe_autoname_session(config.clone());
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
                None => writeln!(
                    output,
                    r#"Usage: .agent <agent-name> [session-name] [key=value]..."#
                )?,
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
                                abort_signal.clone(),
                                input,
                                true,
                                async_manager,
                                persistent_manager,
                                pending_async_context,
                                max_resume,
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
                    _ => writeln!(output, r#"Usage: .edit <config|agent|session|rag-docs>"#)?,
                }
            }
            ".compact" => match args {
                Some("session") => {
                    abortable_run_with_spinner(
                        Config::compact_session(config),
                        "Compacting",
                        abort_signal.clone(),
                    )
                    .await?;
                    writeln!(output, "✓ Successfully compacted the session.")?;
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
            ".mcp" => match split_first_arg(args) {
                Some(("list", _)) => {
                    let servers = Config::mcp_list_servers(config);
                    if servers.is_empty() {
                        writeln!(output, "No MCP servers configured")?;
                    } else {
                        writeln!(output, "MCP Servers:")?;
                        for name in servers {
                            writeln!(output, "  {}", name)?;
                        }
                    }
                }
                Some(("connect", name)) => {
                    let name = name.map(|n| n.trim()).unwrap_or("");
                    if name.is_empty() {
                        writeln!(output, "Usage: .mcp connect <server>")?;
                    } else {
                        Config::mcp_connect_server(config, name).await?;
                        writeln!(output, "Connected to MCP server '{}'", name)?;
                    }
                }
                Some(("disconnect", name)) => {
                    let name = name.map(|n| n.trim()).unwrap_or("");
                    if name.is_empty() {
                        crate::utils::emit_info("Usage: .mcp disconnect <server>".to_string());
                    } else {
                        Config::mcp_disconnect_server(config, name).await?;
                        crate::utils::emit_info(format!("Disconnected from MCP server '{name}'"));
                    }
                }
                Some(("tools", name)) => {
                    let mcp_manager = config.read().mcp_manager.clone();
                    if let Some(manager) = mcp_manager {
                        let tools = match name {
                            Some(n) if !n.trim().is_empty() => {
                                manager.get_server_tools(n.trim()).await?
                            }
                            _ => manager.get_all_tools().await,
                        };
                        if tools.is_empty() {
                            writeln!(output, "No MCP tools available")?;
                        } else {
                            writeln!(output, "MCP Tools:")?;
                            for tool in tools {
                                writeln!(output, "  {} - {}", tool.name, tool.description)?;
                            }
                        }
                    } else {
                        writeln!(output, "MCP is not configured")?;
                    }
                }
                Some(("roots", name)) => {
                    let name = name.map(|n| n.trim()).unwrap_or("");
                    if name.is_empty() {
                        writeln!(output, "Usage: .mcp roots <server>")?;
                    } else {
                        let roots = Config::mcp_get_roots(config, name)?;
                        if roots.is_empty() {
                            writeln!(output, "No roots for MCP server '{}'", name)?;
                        } else {
                            writeln!(output, "MCP Roots for '{}':", name)?;
                            for root in roots {
                                writeln!(output, "  {}", root)?;
                            }
                        }
                    }
                }
                Some(("add-root", name_and_root)) => {
                    let name_and_root = name_and_root.map(|n| n.trim()).unwrap_or("");
                    if let Some((name, root)) = name_and_root.split_once(' ') {
                        let (name, root) = (name.trim(), root.trim());
                        if name.is_empty() || root.is_empty() {
                            writeln!(output, "Usage: .mcp add-root <server> <root>")?;
                        } else {
                            Config::mcp_add_root(config, name, root).await?;
                            writeln!(output, "Added root '{}' to MCP server '{}'", root, name)?;
                        }
                    } else {
                        writeln!(output, "Usage: .mcp add-root <server> <root>")?;
                    }
                }
                Some(("remove-root", name_and_root)) => {
                    let name_and_root = name_and_root.map(|n| n.trim()).unwrap_or("");
                    if let Some((name, root)) = name_and_root.split_once(' ') {
                        let (name, root) = (name.trim(), root.trim());
                        if name.is_empty() || root.is_empty() {
                            writeln!(output, "Usage: .mcp remove-root <server> <root>")?;
                        } else {
                            Config::mcp_remove_root(config, name, root).await?;
                            writeln!(output, "Removed root '{}' from MCP server '{}'", root, name)?;
                        }
                    } else {
                        writeln!(output, "Usage: .mcp remove-root <server> <root>")?;
                    }
                }
                _ => {
                    writeln!(
                        output,
                        r#"Usage: .mcp <command>

Commands:
  .mcp list                    - List configured MCP servers
  .mcp connect <server>        - Connect to an MCP server
  .mcp disconnect <server>     - Disconnect from an MCP server
  .mcp tools [server]          - List available MCP tools
  .mcp roots <server>          - List roots for an MCP server
  .mcp add-root <server> <root> - Add a root to an MCP server
  .mcp remove-root <server> <root> - Remove a root from an MCP server"#
                    )?;
                }
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
                        abort_signal.clone(),
                        input,
                        true,
                        async_manager,
                        persistent_manager,
                        pending_async_context,
                        max_resume,
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
                    abort_signal.clone(),
                    input,
                    true,
                    async_manager,
                    persistent_manager,
                    pending_async_context,
                    max_resume,
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
                    abort_signal.clone(),
                    input,
                    true,
                    async_manager,
                    persistent_manager,
                    pending_async_context,
                    max_resume,
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
                Some(args) => {
                    Config::delete(config, args)?;
                }
                _ => writeln!(
                    output,
                    "Usage: .delete <agent|session|rag|macro|agent-data>"
                )?,
            },
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
                Some("session") => {
                    if config.read().agent.is_some() {
                        config.write().exit_agent_session()?;
                    } else {
                        config.write().exit_session()?;
                    }
                }
                Some("rag") => {
                    config.write().exit_rag()?;
                }
                Some("agent") => {
                    config.write().exit_agent()?;
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
            let (hooks, session_id) = {
                let config = config.read();
                (
                    config.resolved_hooks(),
                    config
                        .session
                        .as_ref()
                        .map(|session| session.name())
                        .unwrap_or("default")
                        .to_string(),
                )
            };
            let cwd = env::current_dir().unwrap_or_default();
            let event = HookEvent::UserPromptSubmit {
                prompt: line.to_string(),
            };
            let outcome = dispatch_hooks_with_managers(
                &event,
                &hooks.entries,
                &session_id,
                &cwd,
                Some(async_manager),
                Some(persistent_manager),
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
                        abort_signal.clone(),
                        input,
                        true,
                        async_manager,
                        persistent_manager,
                        pending_async_context,
                        hooks.max_resume.unwrap_or(5),
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
    abort_signal: AbortSignal,
    input: Input,
    with_embeddings: bool,
    async_manager: &mut AsyncHookManager,
    persistent_manager: &std::sync::Arc<tokio::sync::Mutex<PersistentHookManager>>,
    pending_async_context: &mut Option<String>,
    max_resume: u32,
) -> Result<()> {
    ask_inner(
        config,
        abort_signal,
        input,
        with_embeddings,
        async_manager,
        persistent_manager,
        pending_async_context,
        0,
        max_resume,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn ask_inner(
    config: &GlobalConfig,
    abort_signal: AbortSignal,
    input: Input,
    with_embeddings: bool,
    async_manager: &mut AsyncHookManager,
    persistent_manager: &std::sync::Arc<tokio::sync::Mutex<PersistentHookManager>>,
    pending_async_context: &mut Option<String>,
    resume_count: u32,
    max_resume: u32,
) -> Result<()> {
    // Wrap the &mut managers into Arc<Mutex> for the unified loop.
    // This is safe because ask_inner is called once per user turn (not concurrently).
    let am = std::mem::take(async_manager);
    let am_arc = Arc::new(tokio::sync::Mutex::new(am));
    let pending = std::mem::take(pending_async_context);
    let pending_arc = Arc::new(tokio::sync::Mutex::new(pending));

    let ctx = crate::agent_loop::AgentLoopContext {
        config: config.clone(),
        abort_signal: abort_signal.clone(),
        async_manager: am_arc.clone(),
        persistent_manager: persistent_manager.clone(),
        call_fn: None,
        on_tool_round: None,
        on_text_response: None,
        initial_with_embeddings: with_embeddings,
        initial_resume_count: resume_count,
        max_resume: Some(max_resume),
        pending_async_context: Some(pending_arc.clone()),
    };

    let result = crate::agent_loop::run_agent_loop(&ctx, input).await;

    let mut guard = am_arc.lock().await;
    *async_manager = std::mem::take(&mut *guard);
    let mut pending_guard = pending_arc.lock().await;
    *pending_async_context = std::mem::take(&mut *pending_guard);

    result
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
