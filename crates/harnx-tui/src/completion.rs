//! Completion candidates for the `.`-prefixed commands.

use std::collections::HashSet;

/// Completions for a partial first word such as `.inf`.
///
/// A dispatch name can appear more than once in `COMMANDS`, since commands
/// sharing a name differ only in the arguments they document. Offering it twice
/// would put a duplicate in the picker, so only the first is kept.
pub(crate) fn command_name_completions(filter: &str) -> Vec<(String, Option<String>)> {
    let mut seen = HashSet::new();
    harnx_runtime::commands::COMMANDS
        .iter()
        .filter(|command| command.name.starts_with(filter) && seen.insert(command.name))
        .map(|command| {
            (
                format!("{} ", command.name),
                Some(command.description.to_string()),
            )
        })
        .collect()
}

/// Query the current session's worker, never the process-wide tool inventory.
pub(crate) async fn session_tool_completions(
    config: &harnx_runtime::config::GlobalConfig,
    worker: &std::sync::Arc<
        tokio::sync::Mutex<Option<harnx_runtime::local_orchestrator::LocalWorkerSupervisor>>,
    >,
    filter: &str,
) -> Vec<(String, Option<String>)> {
    let abort = harnx_runtime::utils::create_abort_signal();
    let request = harnx_runtime::operator_tools::run_session_tool_command(
        config,
        &abort,
        harnx_runtime::operator_tools::OperatorToolCommand::List { pattern: None },
        true,
        worker,
    );
    let Ok(Ok(reply)) = tokio::time::timeout(std::time::Duration::from_secs(2), request).await
    else {
        abort.set_ctrlc();
        return Vec::new();
    };
    if reply.error.is_some() {
        return Vec::new();
    }
    let Ok(tools) = serde_json::from_str::<Vec<serde_json::Value>>(&reply.output) else {
        return Vec::new();
    };
    tools
        .iter()
        .filter_map(|tool| {
            let name = tool["name"].as_str()?;
            name.starts_with(filter).then(|| {
                (
                    name.to_owned(),
                    tool["description"].as_str().map(str::to_owned),
                )
            })
        })
        .collect()
}

// Keep a trailing empty word so completion after a space targets the next argument.
pub(crate) fn command_parts(line: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = None;
    for (i, ch) in line.char_indices() {
        if ch == ' ' {
            if let Some(s) = start.take() {
                parts.push(&line[s..i]);
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    match start {
        Some(s) => parts.push(&line[s..]),
        None => parts.push(""),
    }
    parts
}

impl crate::types::Tui {
    pub(super) async fn command_argument_completions(
        &self,
        cmd: &str,
        args: Vec<&str>,
    ) -> Vec<(String, Option<String>)> {
        if let [subject, filter] = args.as_slice() {
            if matches!(
                (cmd, *subject),
                (".info" | ".call", "tool") | (".list", "tools")
            ) {
                return session_tool_completions(&self.config, &self.local_worker, filter).await;
            }
        }
        let filter = args.last().copied().unwrap_or("");
        match cmd {
            ".attach" => return attachment_path_completions(filter).await,
            ".detach" => {
                return self
                    .app
                    .attachments
                    .iter()
                    .filter(|a| a.display_name.starts_with(filter))
                    .map(|a| (a.display_name.clone(), None))
                    .collect()
            }
            _ => {}
        }
        let (cmd, args) = match (cmd, args.as_slice()) {
            (".info" | ".dump", ["session", _, ..]) => (".session", args[1..].to_vec()),
            _ => (cmd, args),
        };
        if cmd == ".agent" && args.iter().all(|arg| arg.is_empty()) {
            return vec![];
        }
        if cmd == ".session" && args.len() == 2 {
            let cfg = self.config.read().clone();
            let sessions = cfg.list_sessions_for_completion(args[0]).await;
            return harnx_runtime::utils::fuzzy_filter(
                sessions.into_iter().map(|id| (id, None)).collect(),
                |value| value.0.as_str(),
                args[1],
            );
        }
        // Display names respect the frontend's configured cluster.
        let precomputed_agents = if matches!(cmd, ".agent" | ".session") && args.len() == 1 {
            Self::assistant_agents_for_display(&self.config).await
        } else {
            Vec::new()
        };
        self.config
            .read()
            .command_complete(cmd, &args, precomputed_agents)
    }
}

async fn attachment_path_completions(filter: &str) -> Vec<(String, Option<String>)> {
    let (dir_path, prefix) = attachment_filter_path(filter);
    let Ok(mut entries) = tokio::fs::read_dir(&dir_path).await else {
        return vec![];
    };
    let mut matches = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&prefix) {
            let full = if dir_path == std::path::Path::new(".") {
                name.clone()
            } else {
                format!("{}/{}", dir_path.display(), name)
            };
            let kind = match entry.file_type().await {
                Ok(file_type) if file_type.is_dir() => Some("dir".to_string()),
                _ => None,
            };
            matches.push((full, kind));
        }
    }
    matches
}

fn attachment_filter_path(filter: &str) -> (std::path::PathBuf, String) {
    if filter.contains('/') || filter.contains('\\') {
        let p = std::path::Path::new(filter);
        let dir_path = p
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf();
        let prefix = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        (dir_path, prefix)
    } else {
        (std::path::PathBuf::from("."), filter.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_words_keep_trailing_argument_and_space_only_boundaries() {
        for (line, expected) in [
            ("", vec![""]),
            ("  ", vec![""]),
            (".call", vec![".call"]),
            (".call ", vec![".call", ""]),
            (".info  tool  écho ", vec![".info", "tool", "écho", ""]),
            (".call\ttool", vec![".call\ttool"]),
            ("ordinary text", vec!["ordinary", "text"]),
        ] {
            assert_eq!(command_parts(line), expected, "{line:?}");
        }
    }

    #[test]
    fn attachment_filter_keeps_relative_directory_and_prefix() {
        assert_eq!(
            attachment_filter_path("fixture"),
            (std::path::PathBuf::from("."), "fixture".into())
        );
        assert_eq!(
            attachment_filter_path("./fixture"),
            (std::path::PathBuf::from("."), "fixture".into())
        );
        assert_eq!(
            attachment_filter_path("dir/fixture"),
            (std::path::PathBuf::from("dir"), "fixture".into())
        );
    }
}
