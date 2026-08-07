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
    (
        ".edit",
        &["config", "agent", "session", "message", "rag-docs"],
    ),
    (
        ".info",
        &[
            "session", "model", "agent", "rag", "tools", "theme", "mcp", "env",
        ],
    ),
    (".mcp", &["list", "connect", "disconnect", "tools"]),
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
                ".session" => map_completion_values(self.list_sessions()),
                ".rag" => map_completion_values(Self::list_rags()),
                ".agent" => map_completion_values(precomputed_agents),
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

    /// Generate tab-completion candidates for session IDs.
    ///
    /// This is the async variant that should be used from async contexts (TUI).
    /// When a remote agent is in context, fetches session IDs from the remote
    /// NATS KV index with a short timeout for snappy completion. Otherwise,
    /// returns local session IDs.
    ///
    /// # Arguments
    /// * `cluster` - The cluster name if a remote agent is active, or `None` for local
    ///
    /// # Returns
    /// Session ID strings suitable for completion. Returns empty vec on timeout/error
    /// (graceful degradation).
    pub async fn list_sessions_for_completion(&self, cluster: Option<&str>) -> Vec<String> {
        match cluster {
            Some(cluster) => {
                // Short timeout to keep completion snappy; on timeout/error,
                // fall back to empty vec (graceful degradation).
                match tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    self.list_remote_sessions_with_meta(cluster),
                )
                .await
                {
                    Ok(Ok(sessions)) => sessions.into_iter().map(|s| s.id).collect(),
                    Ok(Err(e)) => {
                        log::debug!("Remote session completion failed: {:#}", e);
                        vec![]
                    }
                    Err(_) => {
                        log::debug!("Remote session completion timed out");
                        vec![]
                    }
                }
            }
            None => {
                let config = self.clone();
                tokio::task::spawn_blocking(move || config.list_sessions())
                    .await
                    .unwrap_or_else(|error| {
                        log::debug!("Local session completion task failed: {error}");
                        vec![]
                    })
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Test that list_sessions_for_completion returns local sessions when cluster is None.
    ///
    /// Creates a temp sessions dir with known session files, verifies that
    /// `list_sessions_for_completion(None)` returns those session IDs.
    #[tokio::test]
    async fn test_list_sessions_for_completion_local_branch() {
        // Create a temp directory to act as sessions_dir
        let temp = tempfile::TempDir::new().expect("temp dir");
        let sessions_dir = temp.path();

        // Create session files: session-alpha.yaml, session-beta.yaml
        let session_alpha = sessions_dir.join("session-alpha.yaml");
        let session_beta = sessions_dir.join("session-beta.yaml");
        std::fs::File::create(&session_alpha)
            .expect("create session-alpha")
            .write_all(b"id: session-alpha\n")
            .expect("write session-alpha");
        std::fs::File::create(&session_beta)
            .expect("create session-beta")
            .write_all(b"id: session-beta\n")
            .expect("write session-beta");

        // Build a minimal Config with sessions_dir_override pointing to temp
        let config = Config {
            sessions_dir_override: Some(sessions_dir.to_path_buf()),
            ..Config::default()
        };

        // WHEN cluster = None, local branch fires
        let result = config.list_sessions_for_completion(None).await;

        // THEN result contains our session IDs (sorted alphabetically)
        assert_eq!(
            result,
            vec!["session-alpha", "session-beta"],
            "cluster=None should return local session IDs from list_sessions()"
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
            .list_sessions_for_completion(Some("bogus-unreachable-cluster-xyz-9f8e7d"))
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
