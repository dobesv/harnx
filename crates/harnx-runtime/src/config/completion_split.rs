//! Command-line tab completion extracted from config/mod.rs for code health.
use super::*;

impl Config {
    pub fn command_complete(
        &self,
        cmd: &str,
        args: &[&str],
        precomputed_agents: Vec<String>,
    ) -> Vec<(String, Option<String>)> {
        let mut values: Vec<(String, Option<String>)> = vec![];
        let filter = args.last().unwrap_or(&"");
        if args.len() == 1 {
            values = match cmd {
                ".model" => list_models(&self.clients, ModelType::Chat)
                    .into_iter()
                    .map(|v| (v.id(), Some(v.description())))
                    .collect(),
                ".session" => map_completion_values(self.list_sessions()),
                ".rag" => map_completion_values(Self::list_rags()),
                ".agent" => map_completion_values(precomputed_agents),
                ".macro" => map_completion_values(Self::list_macros()),
                ".info" => map_completion_values(vec![
                    "session", "model", "agent", "rag", "tools", "theme",
                ]),
                ".mcp" => map_completion_values(vec![
                    "list",
                    "connect",
                    "disconnect",
                    "tools",
                    "roots",
                    "add-root",
                    "remove-root",
                ]),
                ".use" => map_completion_values(vec!["tool"]),
                ".drop" => map_completion_values(vec!["tool"]),
                ".starter" => match &self.agent {
                    Some(agent) => agent
                        .conversation_staters()
                        .iter()
                        .enumerate()
                        .map(|(i, v)| ((i + 1).to_string(), Some(v.to_string())))
                        .collect(),
                    None => vec![],
                },
                ".set" => {
                    let mut values = vec![
                        "temperature",
                        "top_p",
                        "use_tools",
                        "save_session",
                        "compress_threshold",
                        "compaction_agent",
                        "model_fallbacks",
                        "rag_reranker_model",
                        "rag_top_k",
                        "max_output_tokens",
                        "dry_run",
                        "tool_use",
                        "stream",
                        "save",
                        "highlight",
                    ];
                    values.sort_unstable();
                    values
                        .into_iter()
                        .map(|v| (format!("{v} "), None))
                        .collect()
                }
                ".delete" => {
                    map_completion_values(vec!["agent", "session", "rag", "macro", "agent-data"])
                }
                _ => vec![],
            };
        } else if cmd == ".set" && args.len() == 2 {
            let candidates = match args[0] {
                "max_output_tokens" => match self.current_model().max_output_tokens() {
                    Some(v) => vec![v.to_string()],
                    None => vec![],
                },
                "dry_run" => complete_bool(self.dry_run),
                "stream" => complete_bool(self.stream),
                "save" => complete_bool(self.save),
                "tool_use" => complete_bool(self.tool_use),
                "use_tools" => {
                    let mut prefix = String::new();
                    let mut ignores = HashSet::new();
                    if let Some((v, _)) = args[1].rsplit_once(',') {
                        ignores = v.split(',').collect();
                        prefix = format!("{v},");
                    }
                    let mut values = vec![];
                    if prefix.is_empty() {
                        values.push("*".to_string());
                    }
                    values.extend(
                        self.tool_declarations_for_use_tools(
                            Some("*"),
                            self.active_package().as_deref(),
                        )
                        .0
                        .iter()
                        .map(|v| v.name.clone()),
                    );
                    values.extend(self.toolsets.keys().map(|v| v.to_string()));
                    values
                        .into_iter()
                        .filter(|v| !ignores.contains(v.as_str()))
                        .map(|v| format!("{prefix}{v}"))
                        .collect()
                }
                "save_session" => {
                    let save_session = if let Some(session) = &self.session {
                        session.save_session()
                    } else {
                        self.save_session
                    };
                    complete_option_bool(save_session)
                }
                "rag_reranker_model" => list_models(&self.clients, ModelType::Reranker)
                    .iter()
                    .map(|v| v.id())
                    .collect(),
                "highlight" => complete_bool(self.highlight),
                _ => vec![],
            };
            values = candidates.into_iter().map(|v| (v, None)).collect();
        } else if cmd == ".use" && args.len() == 2 && args[0] == "tool" {
            let mut candidates: Vec<String> = self
                .tool_declarations_for_use_tools(Some("*"), self.active_package().as_deref())
                .0
                .iter()
                .map(|v| v.name.clone())
                .collect();
            candidates.extend(self.toolsets.keys().map(|v| v.to_string()));
            if let Some(manager) = &self.mcp_manager {
                for name in manager.list_servers() {
                    candidates.push(format!("{name}_*"));
                }
            }
            let active = self.active_tool_names();
            values = candidates
                .into_iter()
                .filter(|v| !active.contains(v))
                .map(|v| (v, None))
                .collect();
        } else if cmd == ".drop" && args.len() == 2 && args[0] == "tool" {
            let agent = self.extract_agent();
            let current = agent.use_tools().unwrap_or_default();
            values = current.into_iter().map(|s| (s, None)).collect();
        } else if cmd == ".mcp" && args.len() == 2 {
            let subcmd = args[0];
            if matches!(
                subcmd,
                "connect" | "disconnect" | "tools" | "roots" | "add-root" | "remove-root"
            ) {
                let servers = Self::mcp_list_servers_from_config(self);
                values = servers.into_iter().map(|v| (v, None)).collect();
            }
        } else if cmd == ".agent" {
            if args.len() == 2 {
                let dir = Self::agent_data_dir(args[0]).join(paths::SESSIONS_DIR_NAME);
                values = list_file_names(dir, ".yaml")
                    .into_iter()
                    .map(|v| (v, None))
                    .collect();
            }
            values.extend(complete_agent_variables(args[0]));
        };
        fuzzy_filter(values, |v| v.0.as_str(), filter)
    }
}

fn complete_bool(value: bool) -> Vec<String> {
    vec![(!value).to_string()]
}

fn complete_option_bool(value: Option<bool>) -> Vec<String> {
    match value {
        Some(true) => vec!["false".to_string(), "null".to_string()],
        Some(false) => vec!["true".to_string(), "null".to_string()],
        None => vec!["true".to_string(), "false".to_string()],
    }
}
