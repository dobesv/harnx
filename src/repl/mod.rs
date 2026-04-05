mod completer;
mod highlighter;
mod prompt;

use self::completer::ReplCompleter;
use self::highlighter::ReplHighlighter;
use self::prompt::ReplPrompt;

use crate::client::{call_chat_completions, call_chat_completions_streaming};
use crate::config::{
    macro_execute, AgentVariables, AssertState, Config, GlobalConfig, Input, LastMessage,
    StateFlags,
};
use crate::hooks::{
    dispatch_hooks_with_count_and_manager, dispatch_hooks_with_managers, drain_async_results,
    inject_pending_async_context, AsyncHookManager, HookEvent, HookResultControl,
    PersistentHookManager,
};
use crate::render::render_error;
use crate::utils::{
    abortable_run_with_spinner, create_abort_signal, dimmed_text, set_text, temp_file, AbortSignal,
};

use anyhow::{anyhow, bail, Context, Result};
use crossterm::cursor::SetCursorStyle;
use fancy_regex::Regex;
use reedline::CursorConfig;
use reedline::{
    default_emacs_keybindings, default_vi_insert_keybindings, default_vi_normal_keybindings,
    ColumnarMenu, EditCommand, EditMode, Emacs, KeyCode, KeyModifiers, Keybindings, Reedline,
    ReedlineEvent, ReedlineMenu, ValidationResult, Validator, Vi,
};
use reedline::{MenuBuilder, Signal};
use std::sync::LazyLock;
use std::{env, process};

const MENU_NAME: &str = "completion_menu";

static REPL_COMMANDS: LazyLock<[ReplCommand; 37]> = LazyLock::new(|| {
    [
        ReplCommand::new(".help", "Show this help guide", AssertState::pass()),
        ReplCommand::new(".info", "Show system info", AssertState::pass()),
        ReplCommand::new(
            ".info tools",
            "List all available tools and their status",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".use tool",
            "Add a tool or toolset to the active tools",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".drop tool",
            "Remove a tool or toolset from the active tools",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".edit config",
            "Modify configuration file",
            AssertState::False(StateFlags::AGENT),
        ),
        ReplCommand::new(".model", "Switch LLM model", AssertState::pass()),
        ReplCommand::new(
            ".prompt",
            "Set a temporary agent using a prompt",
            AssertState::False(StateFlags::SESSION | StateFlags::AGENT),
        ),
        ReplCommand::new(
            ".edit agent",
            "Modify current agent",
            AssertState::TrueFalse(StateFlags::AGENT, StateFlags::SESSION),
        ),
        ReplCommand::new(
            ".save agent",
            "Save current agent to file",
            AssertState::TrueFalse(
                StateFlags::AGENT,
                StateFlags::SESSION_EMPTY | StateFlags::SESSION,
            ),
        ),
        ReplCommand::new(
            ".session",
            "Start or switch to a session",
            AssertState::False(StateFlags::SESSION_EMPTY | StateFlags::SESSION),
        ),
        ReplCommand::new(
            ".empty session",
            "Clear session messages",
            AssertState::True(StateFlags::SESSION),
        ),
        ReplCommand::new(
            ".reset repl",
            "Reset session to initial state (re-expands variables)",
            AssertState::True(StateFlags::SESSION_EMPTY | StateFlags::SESSION),
        ),
        ReplCommand::new(
            ".compress session",
            "Compress session messages",
            AssertState::True(StateFlags::SESSION),
        ),
        ReplCommand::new(
            ".info session",
            "Show session info",
            AssertState::True(StateFlags::SESSION_EMPTY | StateFlags::SESSION),
        ),
        ReplCommand::new(
            ".edit session",
            "Modify current session",
            AssertState::True(StateFlags::SESSION_EMPTY | StateFlags::SESSION),
        ),
        ReplCommand::new(
            ".save session",
            "Save current session to file",
            AssertState::True(StateFlags::SESSION_EMPTY | StateFlags::SESSION),
        ),
        ReplCommand::new(
            ".exit session",
            "Exit active session",
            AssertState::True(StateFlags::SESSION_EMPTY | StateFlags::SESSION),
        ),
        ReplCommand::new(".agent", "Use an agent", AssertState::bare()),
        ReplCommand::new(
            ".starter",
            "Use a conversation starter",
            AssertState::True(StateFlags::AGENT),
        ),
        ReplCommand::new(
            ".info agent",
            "Show agent info",
            AssertState::True(StateFlags::AGENT),
        ),
        ReplCommand::new(
            ".exit agent",
            "Leave agent",
            AssertState::True(StateFlags::AGENT),
        ),
        ReplCommand::new(
            ".rag",
            "Initialize or access RAG",
            AssertState::False(StateFlags::AGENT),
        ),
        ReplCommand::new(
            ".edit rag-docs",
            "Add or remove documents from an existing RAG",
            AssertState::TrueFalse(StateFlags::RAG, StateFlags::AGENT),
        ),
        ReplCommand::new(
            ".rebuild rag",
            "Rebuild RAG for document changes",
            AssertState::True(StateFlags::RAG),
        ),
        ReplCommand::new(
            ".sources rag",
            "Show citation sources used in last query",
            AssertState::True(StateFlags::RAG),
        ),
        ReplCommand::new(
            ".info rag",
            "Show RAG info",
            AssertState::True(StateFlags::RAG),
        ),
        ReplCommand::new(
            ".exit rag",
            "Leave RAG",
            AssertState::TrueFalse(StateFlags::RAG, StateFlags::AGENT),
        ),
        ReplCommand::new(".macro", "Execute a macro", AssertState::pass()),
        ReplCommand::new(".mcp", "Manage MCP servers", AssertState::pass()),
        ReplCommand::new(
            ".file",
            "Include files, directories, URLs or commands",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".continue",
            "Continue previous response",
            AssertState::pass(),
        ),
        ReplCommand::new(
            ".regenerate",
            "Regenerate last response",
            AssertState::pass(),
        ),
        ReplCommand::new(".copy", "Copy last response", AssertState::pass()),
        ReplCommand::new(".set", "Modify runtime settings", AssertState::pass()),
        ReplCommand::new(
            ".delete",
            "Delete agents, sessions, RAGs, or macros",
            AssertState::pass(),
        ),
        ReplCommand::new(".exit", "Exit REPL", AssertState::pass()),
    ]
});
static COMMAND_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(\.\S*)\s*").unwrap());
static MULTILINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^\s*:::\s*(.*)\s*:::\s*$").unwrap());

pub struct Repl {
    config: GlobalConfig,
    editor: Reedline,
    prompt: ReplPrompt,
    abort_signal: AbortSignal,
    async_manager: AsyncHookManager,
    persistent_manager: std::sync::Arc<tokio::sync::Mutex<PersistentHookManager>>,
    pending_async_context: Option<String>,
}

impl Repl {
    pub fn init(
        config: &GlobalConfig,
        async_manager: AsyncHookManager,
        persistent_manager: std::sync::Arc<tokio::sync::Mutex<PersistentHookManager>>,
    ) -> Result<Self> {
        let editor = Self::create_editor(config)?;

        let prompt = ReplPrompt::new(config);
        let abort_signal = create_abort_signal();

        Ok(Self {
            config: config.clone(),
            editor,
            prompt,
            abort_signal,
            async_manager,
            persistent_manager,
            pending_async_context: None,
        })
    }

    pub fn async_manager(&self) -> &AsyncHookManager {
        &self.async_manager
    }

    pub async fn run(&mut self) -> Result<()> {
        if AssertState::False(StateFlags::AGENT | StateFlags::RAG)
            .assert(self.config.read().state())
        {
            print!(
                r#"Welcome to {} {}
Type ".help" for additional help.
"#,
                env!("CARGO_CRATE_NAME"),
                env!("CARGO_PKG_VERSION"),
            )
        }

        // Print initial session/agent status line
        {
            let status = self.config.read().render_status_line(true);
            if !status.is_empty() {
                eprintln!("{}", dimmed_text(&status));
            }
        }

        loop {
            if self.abort_signal.aborted_ctrld() {
                break;
            }
            if self.process_pending_async_resume().await? {
                continue;
            }
            let sig = self.editor.read_line(&self.prompt);
            match sig {
                Ok(Signal::Success(line)) => {
                    self.abort_signal.reset();
                    match run_repl_command(
                        &self.config,
                        self.abort_signal.clone(),
                        &line,
                        &mut self.async_manager,
                        &self.persistent_manager,
                        &mut self.pending_async_context,
                    )
                    .await
                    {
                        Ok(exit) => {
                            if exit {
                                break;
                            }
                        }
                        Err(err) => {
                            render_error(err);
                            println!()
                        }
                    }
                }
                Ok(Signal::CtrlC) => {
                    self.abort_signal.set_ctrlc();
                    println!("(To exit, press Ctrl+D or enter \".exit\")\n");
                }
                Ok(Signal::CtrlD) => {
                    self.abort_signal.set_ctrld();
                    break;
                }
                _ => {}
            }
        }
        self.config.write().exit_session()?;
        Ok(())
    }

    async fn process_pending_async_resume(&mut self) -> Result<bool> {
        let (should_resume, max_resume) = {
            let config = self.config.read();
            let hooks = config.resolved_hooks();
            (
                drain_async_results(&mut self.async_manager, &mut self.pending_async_context),
                hooks.max_resume.unwrap_or(5),
            )
        };
        if !should_resume {
            return Ok(false);
        }

        if self.abort_signal.aborted() {
            return Ok(true);
        }

        let context = self
            .pending_async_context
            .take()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "Continue working on pending tasks.".to_string());
        let input = Input::from_str(&self.config, &context, None);
        ask(
            &self.config,
            self.abort_signal.clone(),
            input,
            true,
            &mut self.async_manager,
            &self.persistent_manager,
            &mut self.pending_async_context,
            max_resume,
        )
        .await?;
        Ok(true)
    }

    fn create_editor(config: &GlobalConfig) -> Result<Reedline> {
        let completer = ReplCompleter::new(config);
        let highlighter = ReplHighlighter::new(config);
        let menu = Self::create_menu();
        let edit_mode = Self::create_edit_mode(config);
        let cursor_config = CursorConfig {
            vi_insert: Some(SetCursorStyle::BlinkingBar),
            vi_normal: Some(SetCursorStyle::SteadyBlock),
            emacs: None,
        };
        let mut editor = Reedline::create()
            .with_completer(Box::new(completer))
            .with_highlighter(Box::new(highlighter))
            .with_menu(menu)
            .with_edit_mode(edit_mode)
            .with_cursor_config(cursor_config)
            .with_quick_completions(true)
            .with_partial_completions(true)
            .use_bracketed_paste(true)
            .with_validator(Box::new(ReplValidator))
            .with_ansi_colors(true);

        if let Ok(cmd) = config.read().editor() {
            let temp_file = temp_file("-repl-", ".md");
            let command = process::Command::new(cmd);
            editor = editor.with_buffer_editor(command, temp_file);
        }

        Ok(editor)
    }

    fn extra_keybindings(keybindings: &mut Keybindings) {
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Tab,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Menu(MENU_NAME.to_string()),
                ReedlineEvent::MenuNext,
            ]),
        );
        keybindings.add_binding(
            KeyModifiers::SHIFT,
            KeyCode::BackTab,
            ReedlineEvent::MenuPrevious,
        );
        keybindings.add_binding(
            KeyModifiers::CONTROL,
            KeyCode::Enter,
            ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
        );
        keybindings.add_binding(
            KeyModifiers::CONTROL,
            KeyCode::Char('j'),
            ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
        );
    }

    fn create_edit_mode(config: &GlobalConfig) -> Box<dyn EditMode> {
        let edit_mode: Box<dyn EditMode> = if config.read().keybindings == "vi" {
            let mut insert_keybindings = default_vi_insert_keybindings();
            Self::extra_keybindings(&mut insert_keybindings);
            Box::new(Vi::new(insert_keybindings, default_vi_normal_keybindings()))
        } else {
            let mut keybindings = default_emacs_keybindings();
            Self::extra_keybindings(&mut keybindings);
            Box::new(Emacs::new(keybindings))
        };
        edit_mode
    }

    fn create_menu() -> ReedlineMenu {
        let completion_menu = ColumnarMenu::default().with_name(MENU_NAME);
        ReedlineMenu::EngineCompleter(Box::new(completion_menu))
    }
}

#[derive(Debug, Clone)]
pub struct ReplCommand {
    name: &'static str,
    description: &'static str,
    state: AssertState,
}

impl ReplCommand {
    fn new(name: &'static str, desc: &'static str, state: AssertState) -> Self {
        Self {
            name,
            description: desc,
            state,
        }
    }

    fn is_valid(&self, flags: StateFlags) -> bool {
        self.state.assert(flags)
    }
}

/// A default validator which checks for mismatched quotes and brackets
struct ReplValidator;

impl Validator for ReplValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        let line = line.trim();
        if line.starts_with(r#":::"#) && !line[3..].ends_with(r#":::"#) {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Complete
        }
    }
}

pub async fn run_repl_command(
    config: &GlobalConfig,
    abort_signal: AbortSignal,
    mut line: &str,
    async_manager: &mut AsyncHookManager,
    persistent_manager: &std::sync::Arc<tokio::sync::Mutex<PersistentHookManager>>,
    pending_async_context: &mut Option<String>,
) -> Result<bool> {
    let max_resume = config.read().resolved_hooks().max_resume.unwrap_or(5);
    if let Ok(Some(captures)) = MULTILINE_RE.captures(line) {
        if let Some(text_match) = captures.get(1) {
            line = text_match.as_str();
        }
    }
    match parse_command(line) {
        Some((cmd, args)) => match cmd {
            ".help" => {
                dump_repl_help();
            }
            ".info" => match args {
                Some("session") => {
                    let info = config.read().session_info()?;
                    print!("{info}");
                }
                Some("rag") => {
                    let info = config.read().rag_info()?;
                    print!("{info}");
                }
                Some("agent") => {
                    let info = config.read().agent_info()?;
                    print!("{info}");
                }
                Some("tools") => {
                    let conf = config.read();
                    let declarations = conf.tool_declarations_for_use_tools(Some("*"));
                    let active_tools = conf.active_tool_names();
                    if declarations.is_empty() {
                        println!("No tools available");
                    } else {
                        for decl in &declarations {
                            let marker = if active_tools.contains(&decl.name) {
                                "●"
                            } else {
                                "○"
                            };
                            println!("  {} {} - {}", marker, decl.name, decl.description);
                        }
                        let active_count = declarations
                            .iter()
                            .filter(|d| active_tools.contains(&d.name))
                            .count();
                        println!("\n{} active / {} total", active_count, declarations.len());
                    }
                }
                Some(_) => unknown_command()?,
                None => {
                    let output = config.read().sysinfo()?;
                    print!("{output}");
                }
            },
            ".model" => match args {
                Some(name) => {
                    config.write().set_model(name)?;
                }
                None => println!("Usage: .model <name>"),
            },
            ".prompt" => match args {
                Some(text) => {
                    config.write().use_prompt(text)?;
                }
                None => println!("Usage: .prompt <text>..."),
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
                None => {
                    println!(r#"Usage: .agent <agent-name> [session-name] [key=value]..."#)
                }
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
                            println!("{}", dimmed_text(&format!(">> {text}")));
                            let input = Input::from_str(config, &text, None);
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
                    config.read().print_markdown(&banner)?;
                }
            },
            ".save" => match split_first_arg(args) {
                Some(("agent", name)) => {
                    config.write().save_agent(name)?;
                }
                Some(("session", name)) => {
                    config.write().save_session(name)?;
                }
                _ => {
                    println!(r#"Usage: .save <agent|session> [name]"#)
                }
            },
            ".edit" => {
                if config.read().macro_flag {
                    bail!("Cannot perform this operation because you are in a macro")
                }
                match args {
                    Some("config") => {
                        config.read().edit_config()?;
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
                    _ => {
                        println!(r#"Usage: .edit <config|agent|session|rag-docs>"#)
                    }
                }
            }
            ".compress" => match args {
                Some("session") => {
                    abortable_run_with_spinner(
                        Config::compress_session(config),
                        "Compressing",
                        abort_signal.clone(),
                    )
                    .await?;
                    println!("✓ Successfully compressed the session.");
                }
                _ => {
                    println!(r#"Usage: .compress session"#)
                }
            },
            ".empty" => match args {
                Some("session") => {
                    config.write().empty_session()?;
                }
                _ => {
                    println!(r#"Usage: .empty session"#)
                }
            },
            ".reset" => match args {
                Some("repl") => {
                    config.write().reset_session()?;
                }
                _ => {
                    println!(r#"Usage: .reset repl"#)
                }
            },
            ".rebuild" => match args {
                Some("rag") => {
                    Config::rebuild_rag(config, abort_signal.clone()).await?;
                }
                _ => {
                    println!(r#"Usage: .rebuild rag"#)
                }
            },
            ".sources" => match args {
                Some("rag") => {
                    let output = Config::rag_sources(config)?;
                    println!("{output}");
                }
                _ => {
                    println!(r#"Usage: .sources rag"#)
                }
            },
            ".mcp" => match split_first_arg(args) {
                Some(("list", _)) => {
                    let servers = Config::mcp_list_servers(config);
                    if servers.is_empty() {
                        println!("No MCP servers configured");
                    } else {
                        println!("MCP Servers:");
                        for name in servers {
                            println!("  {}", name);
                        }
                    }
                }
                Some(("connect", name)) => {
                    let name = name.map(|n| n.trim()).unwrap_or("");
                    if name.is_empty() {
                        println!("Usage: .mcp connect <server>");
                    } else {
                        Config::mcp_connect_server(config, name).await?;
                        println!("Connected to MCP server '{}'", name);
                    }
                }
                Some(("disconnect", name)) => {
                    let name = name.map(|n| n.trim()).unwrap_or("");
                    if name.is_empty() {
                        println!("Usage: .mcp disconnect <server>");
                    } else {
                        Config::mcp_disconnect_server(config, name).await?;
                        println!("Disconnected from MCP server '{}'", name);
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
                            println!("No MCP tools available");
                        } else {
                            println!("MCP Tools:");
                            for tool in tools {
                                println!("  {} - {}", tool.name, tool.description);
                            }
                        }
                    } else {
                        println!("MCP is not configured");
                    }
                }
                Some(("roots", name)) => {
                    let name = name.map(|n| n.trim()).unwrap_or("");
                    if name.is_empty() {
                        println!("Usage: .mcp roots <server>");
                    } else {
                        let roots = Config::mcp_get_roots(config, name)?;
                        if roots.is_empty() {
                            println!("No roots for MCP server '{}'", name);
                        } else {
                            println!("MCP Roots for '{}':", name);
                            for root in roots {
                                println!("  {}", root);
                            }
                        }
                    }
                }
                Some(("add-root", name_and_root)) => {
                    let name_and_root = name_and_root.map(|n| n.trim()).unwrap_or("");
                    if let Some((name, root)) = name_and_root.split_once(' ') {
                        let (name, root) = (name.trim(), root.trim());
                        if name.is_empty() || root.is_empty() {
                            println!("Usage: .mcp add-root <server> <root>");
                        } else {
                            Config::mcp_add_root(config, name, root).await?;
                            println!("Added root '{}' to MCP server '{}'", root, name);
                        }
                    } else {
                        println!("Usage: .mcp add-root <server> <root>");
                    }
                }
                Some(("remove-root", name_and_root)) => {
                    let name_and_root = name_and_root.map(|n| n.trim()).unwrap_or("");
                    if let Some((name, root)) = name_and_root.split_once(' ') {
                        let (name, root) = (name.trim(), root.trim());
                        if name.is_empty() || root.is_empty() {
                            println!("Usage: .mcp remove-root <server> <root>");
                        } else {
                            Config::mcp_remove_root(config, name, root).await?;
                            println!("Removed root '{}' from MCP server '{}'", root, name);
                        }
                    } else {
                        println!("Usage: .mcp remove-root <server> <root>");
                    }
                }
                _ => {
                    println!(
                        r#"Usage: .mcp <command>

Commands:
  .mcp list                    - List configured MCP servers
  .mcp connect <server>        - Connect to an MCP server
  .mcp disconnect <server>     - Disconnect from an MCP server
  .mcp tools [server]          - List available MCP tools
  .mcp roots <server>          - List roots for an MCP server
  .mcp add-root <server> <root> - Add a root to an MCP server
  .mcp remove-root <server> <root> - Remove a root from an MCP server"#
                    );
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
                None => println!("Usage: .macro <name> <text>..."),
            },
            ".file" => match args {
                Some(args) => {
                    let (files, text) = split_args_text(args, cfg!(windows));
                    let input = Input::from_files_with_spinner(
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
                None => println!(
                    r#"Usage: .file <file|dir|url|cmd|loader:resource|%%>... [-- <text>...]

.file /tmp/file.txt
.file src/ Cargo.toml -- analyze
.file https://example.com/file.txt -- summarize
.file https://example.com/image.png -- recognize text
.file `git diff` -- Generate git commit message
.file jina:https://example.com
.file %% -- translate last reply to english"#
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
                input.set_regenerate();
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
                        println!(
                            "Usage: .use tool <name>  (tool name, toolset name, or <server>_*)"
                        );
                    } else {
                        let mut conf = config.write();
                        let current = conf.extract_agent().use_tools().unwrap_or_default();
                        if current.iter().any(|v| v == name) {
                            println!("'{}' is already in use_tools", name);
                        } else {
                            let mut new_items = current;
                            new_items.push(name.to_string());
                            conf.set_use_tools(Some(new_items));
                            println!("Added '{}' to use_tools", name);
                        }
                    }
                }
                _ => println!("Usage: .use tool <name>"),
            },
            ".drop" => match split_first_arg(args) {
                Some(("tool", name)) => {
                    let name = name.map(|n| n.trim()).unwrap_or("");
                    if name.is_empty() {
                        println!("Usage: .drop tool <name>");
                    } else {
                        let mut conf = config.write();
                        let current = conf.extract_agent().use_tools().unwrap_or_default();
                        if !current.iter().any(|v| v == name) {
                            println!("'{}' is not in use_tools", name);
                        } else {
                            let remaining: Vec<String> =
                                current.into_iter().filter(|i| i != name).collect();
                            let new_value = if remaining.is_empty() {
                                None
                            } else {
                                Some(remaining)
                            };
                            conf.set_use_tools(new_value);
                            println!("Removed '{}' from use_tools", name);
                        }
                    }
                }
                _ => println!("Usage: .drop tool <name>"),
            },
            ".set" => match args {
                Some(args) => {
                    Config::update(config, args)?;
                }
                _ => {
                    println!("Usage: .set <key> <value>...")
                }
            },
            ".delete" => match args {
                Some(args) => {
                    Config::delete(config, args)?;
                }
                _ => {
                    println!("Usage: .delete <agent|session|rag|macro|agent-data>")
                }
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
                    return Ok(true);
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
                    let input = Input::from_str(config, &input_text, None);
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

    Ok(false)
}

#[allow(clippy::too_many_arguments)]
#[async_recursion::async_recursion]
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
#[async_recursion::async_recursion]
async fn ask_inner(
    config: &GlobalConfig,
    abort_signal: AbortSignal,
    mut input: Input,
    with_embeddings: bool,
    async_manager: &mut AsyncHookManager,
    persistent_manager: &std::sync::Arc<tokio::sync::Mutex<PersistentHookManager>>,
    pending_async_context: &mut Option<String>,
    resume_count: u32,
    max_resume: u32,
) -> Result<()> {
    if input.is_empty() {
        return Ok(());
    }
    if with_embeddings {
        input.use_embeddings(abort_signal.clone()).await?;
    }
    while config.read().is_compressing_session() {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    drain_async_results(async_manager, pending_async_context);
    inject_pending_async_context(&mut input, pending_async_context);

    let client = input.create_client()?;
    config.write().before_chat_completion(&input)?;
    let (hooks, session_id, cwd) = {
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
    };
    let (output, tool_results, usage) = if input.stream() {
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
                let _ = config
                    .write()
                    .after_chat_completion(&input, "", &[], &Default::default());
                return Err(err);
            }
        }
    } else {
        match call_chat_completions(&input, true, false, client.as_ref(), abort_signal.clone())
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
                let _ = config
                    .write()
                    .after_chat_completion(&input, "", &[], &Default::default());
                return Err(err);
            }
        }
    };
    config
        .write()
        .after_chat_completion(&input, &output, &tool_results, &usage)?;
    if tool_results.is_empty() {
        if !config.read().macro_flag {
            eprintln!();
        }
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
                "Captured Stop hook additional context for later auto-continue in REPL: {additional_context}"
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
                None,
                abort_signal.clone(),
            )
            .await?;
            config.write().empty_session()?;
            let new_input = Input::from_str(config, &switch_agent.prompt, None);
            return Box::pin(ask_inner(
                config,
                abort_signal,
                new_input,
                true,
                async_manager,
                persistent_manager,
                pending_async_context,
                0,
                max_resume,
            ))
            .await;
        }

        ask_inner(
            config,
            abort_signal,
            input.merge_tool_results(output, tool_results),
            false,
            async_manager,
            persistent_manager,
            pending_async_context,
            resume_count,
            max_resume,
        )
        .await
    } else {
        if let Some(stop_outcome) = stop_outcome {
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
                return ask_inner(
                    config,
                    abort_signal,
                    new_input,
                    true,
                    async_manager,
                    persistent_manager,
                    pending_async_context,
                    resume_count + 1,
                    max_resume,
                )
                .await;
            }
        }

        if drain_async_results(async_manager, pending_async_context) && resume_count < max_resume {
            if abort_signal.aborted() {
                return Ok(());
            }

            let context = pending_async_context
                .take()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "Continue working on pending tasks.".to_string());
            let new_input = Input::from_str(config, &context, None);
            return ask_inner(
                config,
                abort_signal,
                new_input,
                true,
                async_manager,
                persistent_manager,
                pending_async_context,
                resume_count + 1,
                max_resume,
            )
            .await;
        }

        Config::maybe_autoname_session(config.clone());
        Config::maybe_compress_session(config.clone());
        Ok(())
    }
}

fn unknown_command() -> Result<()> {
    bail!(r#"Unknown command. Type ".help" for additional help."#);
}

fn dump_repl_help() {
    let head = REPL_COMMANDS
        .iter()
        .map(|cmd| format!("{:<24} {}", cmd.name, cmd.description))
        .collect::<Vec<String>>()
        .join("\n");
    println!(
        r###"{head}

Type ::: to start multi-line editing, type ::: to finish it.
Press Ctrl+O to open an editor for editing the input buffer.
Press Ctrl+C to cancel the response, Ctrl+D to exit the REPL."###,
    );
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
