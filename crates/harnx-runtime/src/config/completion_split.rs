//! Command-line tab completion extracted from config/mod.rs for code health.
use super::*;

/// Commands whose second word is a fixed list.
///
/// Kept as data rather than match arms so `command_complete` only carries the
/// cases that have to compute something.
const FIXED_SUBCOMMANDS: &[(&str, &[&str])] = &[
    (
        ".delete",
        &["agent", "session", "rag", "macro", "agent-data", "message"],
    ),
    (".drop", &["tool"]),
    (".dump", &["session"]),
    (
        ".edit",
        &["config", "agent", "session", "message", "rag-docs"],
    ),
    (
        ".info",
        &["session", "model", "agent", "rag", "tools", "theme", "env"],
    ),
    (".title", &["generate", "now"]),
    (".use", &["tool"]),
];

fn fixed_subcommands(cmd: &str) -> Option<&'static [&'static str]> {
    FIXED_SUBCOMMANDS
        .iter()
        .find(|(name, _)| *name == cmd)
        .map(|(_, subcommands)| *subcommands)
}

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
                ".rag" => map_completion_values(Self::list_rags()),
                ".agent" | ".session" => map_completion_values(precomputed_agents),
                ".macro" => map_completion_values(Self::list_macros()),
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
                _ => fixed_subcommands(cmd)
                    .map(|subcommands| map_completion_values(subcommands.to_vec()))
                    .unwrap_or_default(),
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
        } else if cmd == ".agent" {
            values.extend(complete_agent_variables(args[0]));
        };
        fuzzy_filter(values, |v| v.0.as_str(), filter)
    }

    /// Complete session IDs owned by an explicit agent selector, with a short
    /// timeout so an unreachable broker cannot block interactive completion.
    pub async fn list_sessions_for_completion(&self, agent_ref: &str) -> Vec<String> {
        use harnx_core::agent_ref::AgentRef;
        let (agent, cluster) = match AgentRef::parse(agent_ref) {
            AgentRef::Local(agent) => (agent, super::LOCAL_CLUSTER_KEY.into()),
            AgentRef::Remote { agent, cluster } => (agent, cluster),
        };
        match tokio::time::timeout(
            std::time::Duration::from_millis(500),
            self.list_remote_sessions_with_meta(&cluster),
        )
        .await
        {
            Ok(Ok(sessions)) => sessions
                .into_iter()
                .filter(|session| session.agent_name.as_deref() == Some(agent.as_ref()))
                .map(|session| session.id)
                .collect(),
            Ok(Err(e)) => {
                log::debug!("NATS session completion failed: {:#}", e);
                vec![]
            }
            Err(_) => {
                log::debug!("NATS session completion timed out");
                vec![]
            }
        }
    }
}

fn complete_bool(value: bool) -> Vec<String> {
    vec![(!value).to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_first_argument_completes_agents() {
        let config = Config::default();
        assert_eq!(
            config.command_complete(".session", &["alp"], vec!["alpha".into(), "beta".into()]),
            vec![("alpha".into(), None)],
        );
    }

    /// Test that remote session completion gracefully degrades on unreachable cluster.
    ///
    /// Passes a bogus cluster name that has no NATS server; the method should:
    /// 1. Return an empty Vec (no panic/error)
    /// 2. Complete within the timeout (~500ms), not hang indefinitely.
    #[tokio::test]
    async fn test_remote_completion_graceful_degradation() {
        let config = Config::default();

        // Start timing before the call
        let start = std::time::Instant::now();

        // Call with a bogus-unreachable cluster name (no live NATS at this address)
        let result = config
            .list_sessions_for_completion("alpha@bogus-unreachable-cluster-xyz-9f8e7d")
            .await;

        let elapsed = start.elapsed();

        // THEN: graceful degradation — empty vec, no panic
        assert!(
            result.is_empty(),
            "remote completion on unreachable cluster should return empty vec, got: {:?}",
            result
        );

        // AND: completes within the timeout budget (500ms + margin)
        // The implementation uses 500ms timeout; we allow some overhead.
        assert!(
            elapsed < std::time::Duration::from_millis(1500),
            "remote completion should not hang; took {:?}",
            elapsed
        );
    }
}
