//! Shared transcript framing and metadata rendering for session inspection.

use super::{Config, LOCAL_CLUSTER_KEY};
use crate::client;
use crate::nats_session_log::{self, NatsSessionLog};
use crate::nats_session_metadata::{SessionMetadata, SessionMetadataStore};
use anyhow::{bail, Context, Result};
use harnx_core::model::ModelType;
use harnx_core::session::{Session, SessionLogEntry};
use std::{fmt, str::FromStr};

/// Output format for session metadata and transcripts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SessionFormat {
    #[default]
    Text,
    Yaml,
    /// JSONL transcripts: one object per line, never a JSON array.
    /// Metadata is a single JSON object.
    Json,
}

impl FromStr for SessionFormat {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "text" => Ok(Self::Text),
            "yaml" => Ok(Self::Yaml),
            "json" => Ok(Self::Json),
            _ => bail!("Unknown session format '{value}'; expected text, yaml, or json"),
        }
    }
}

impl fmt::Display for SessionFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Text => "text",
            Self::Yaml => "yaml",
            Self::Json => "json",
        })
    }
}

/// Session inspection command used to report the matching argument syntax.
#[derive(Debug, Clone, Copy)]
pub enum SessionInspectionCommand {
    Info,
    Dump,
}

impl fmt::Display for SessionInspectionCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Info => ".info session",
            Self::Dump => ".dump session",
        })
    }
}

/// Parse the explicit agent, session ID, and optional format used by dot commands.
pub fn parse_session_inspection_args(
    args: &[String],
    command: SessionInspectionCommand,
) -> Result<(String, String, SessionFormat)> {
    let mut positional = Vec::new();
    let mut format = None;
    let mut tokens = args.iter();
    while let Some(token) = tokens.next() {
        if token.starts_with("--format=") || token == "--format" {
            anyhow::ensure!(format.is_none(), "Duplicate --format option");
            let value = token.strip_prefix("--format=").map(Ok).unwrap_or_else(|| {
                tokens
                    .next()
                    .map(String::as_str)
                    .context("Missing value for --format")
            })?;
            format = Some(value.parse()?);
        } else {
            anyhow::ensure!(!token.starts_with('-'), "Unknown session option '{token}'");
            positional.push(token.clone());
        }
    }
    anyhow::ensure!(positional.len() == 2 && positional.iter().all(|value| !value.trim().is_empty()),
        "An explicit agent and session ID are required. Usage: {command} <agent> <id> [--format text|yaml|json]");
    Ok((
        positional.remove(0),
        positional.remove(0),
        format.unwrap_or_default(),
    ))
}

/// Serialize one entry, including control entries, as a YAML document.
/// The leading marker keeps concatenated dump and follow batches valid YAML.
pub fn yaml_doc(entry: &SessionLogEntry) -> Result<String> {
    let yaml = serde_yaml::to_string(entry).context("Failed to render session entry as YAML")?;
    Ok(format!("---\n{yaml}"))
}

/// Serialize entries as a YAML document stream without sequence-number wrappers.
pub fn dump_entries_yaml<'a>(
    entries: impl IntoIterator<Item = &'a SessionLogEntry>,
) -> Result<String> {
    entries.into_iter().map(yaml_doc).collect()
}

/// Serialize one entry, including control entries, as a newline-terminated JSON object.
pub fn jsonl_line(entry: &SessionLogEntry) -> Result<String> {
    let mut json =
        serde_json::to_string(entry).context("Failed to render session entry as JSON")?;
    json.push('\n');
    Ok(json)
}

/// Serialize entries as JSONL, not a JSON array, without sequence-number wrappers.
pub fn dump_entries_jsonl<'a>(
    entries: impl IntoIterator<Item = &'a SessionLogEntry>,
) -> Result<String> {
    entries.into_iter().map(jsonl_line).collect()
}

/// Serialize the full metadata record, including variables, as one YAML document.
pub fn render_metadata_yaml(metadata: &SessionMetadata) -> Result<String> {
    serde_yaml::to_string(metadata).context("Failed to render session metadata as YAML")
}

/// Serialize the full metadata record, including variables, as one compact JSON object.
pub fn render_metadata_json(metadata: &SessionMetadata) -> Result<String> {
    serde_json::to_string(metadata).context("Failed to render session metadata as JSON")
}

/// Resolve an explicit agent selector independently of the active frontend agent.
pub fn resolve_session_agent(agent_ref: &str) -> Result<(String, String)> {
    use harnx_core::agent_ref::AgentRef;
    let (agent, cluster) = match AgentRef::parse(agent_ref) {
        AgentRef::Local(agent) => (agent, LOCAL_CLUSTER_KEY.into()),
        AgentRef::Remote { agent, cluster } => (agent, cluster),
    };
    anyhow::ensure!(
        !agent.trim().is_empty()
            && agent != harnx_core::agent_config::TEMP_AGENT_NAME
            && agent != "__temp__",
        "an explicit named agent is required"
    );
    anyhow::ensure!(!cluster.trim().is_empty(), "cluster must not be empty");
    Ok((agent.into_owned(), cluster.into_owned()))
}

impl Config {
    /// Resolve an explicit agent selector using this frontend's default route.
    pub fn resolve_session_agent(&self, agent_ref: &str) -> Result<(String, String)> {
        use harnx_core::agent_ref::AgentRef;
        let (agent, cluster) = resolve_session_agent(agent_ref)?;
        let cluster = match AgentRef::parse(agent_ref) {
            AgentRef::Local(_) => self.default_cluster_key().to_string(),
            AgentRef::Remote { .. } => cluster,
        };
        Ok((agent, cluster))
    }
}

/// Read one explicitly named agent's metadata and return its broker context.
pub async fn session_metadata_for_agent(
    config: &Config,
    agent_ref: &str,
    session_id: &str,
) -> Result<(async_nats::jetstream::Context, SessionMetadata)> {
    let (agent, cluster) = config.resolve_session_agent(agent_ref)?;
    let jetstream = config.nats_jetstream(&cluster).await?;
    let replicas = config
        .resolve_nats_server(&cluster)
        .await?
        .resolved_replicas();
    let store = SessionMetadataStore::ensure(&jetstream, replicas).await?;
    let metadata = store
        .get_for_agent(session_id, &agent)
        .await?
        .with_context(|| format!("Session '{session_id}' for agent '{agent_ref}' not found"))?
        .metadata;
    Ok((jetstream, metadata))
}

/// Load a named session with its resolved model and transcript-derived token counts.
/// Pass the result to [`super::session::render`] for the runtime metadata view.
/// `None` selects the config's default NATS cluster. Inline agents are unsupported.
pub async fn load_session_for_render(
    config: &Config,
    cluster: Option<&str>,
    session_id: &str,
    expected_agent: &str,
) -> Result<Session> {
    let cluster = cluster.unwrap_or_else(|| config.default_cluster_key());
    let jetstream = config.nats_jetstream(cluster).await?;
    let replicas = config
        .resolve_nats_server(cluster)
        .await?
        .resolved_replicas();
    let store = SessionMetadataStore::ensure(&jetstream, replicas).await?;
    let metadata = store
        .get_for_agent(session_id, expected_agent)
        .await?
        .with_context(|| {
            format!(
                "NATS session '{session_id}' was not found for agent '{expected_agent}'; \
                 only named-agent sessions are supported (inline agents are unsupported)"
            )
        })?
        .metadata;

    let model = match metadata.overrides.model.as_deref() {
        Some(id) => client::retrieve_model(&config.clients, id, ModelType::Chat)?,
        None => config.retrieve_agent(expected_agent)?.model().clone(),
    };
    let mut base = metadata.base_session();
    base.model_id = model.id();
    base.model = model;

    let raw = NatsSessionLog::new(jetstream, metadata.storage_key())
        .load_events_async()
        .await
        .with_context(|| format!("Failed to load NATS session '{session_id}'"))?;
    let mut session =
        nats_session_log::load_session_from_entries_with_metadata(&raw, session_id, base)?;
    session.update_tokens();
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NatsRouting;
    use crate::nats_session_metadata::SessionInitializer;
    use harnx_core::message::{MessageContent, MessageRole};
    use harnx_core::session::ToolOutput;
    use harnx_core::tool::ToolCall;
    use serde::Deserialize;
    use serde_json::{json, Value};

    #[test]
    fn resolve_session_agent_uses_default_route_and_preserves_explicit_cluster() {
        let default_config = Config::default();
        assert_eq!(
            default_config.resolve_session_agent("assistant").unwrap(),
            ("assistant".to_string(), LOCAL_CLUSTER_KEY.to_string())
        );
        assert_eq!(
            default_config
                .resolve_session_agent("assistant@prod")
                .unwrap(),
            ("assistant".to_string(), "prod".to_string())
        );

        let cluster_config = Config {
            nats_routing: NatsRouting::Cluster("remote".to_string()),
            ..Config::default()
        };
        assert_eq!(
            cluster_config.resolve_session_agent("assistant").unwrap(),
            ("assistant".to_string(), "remote".to_string())
        );
        assert_eq!(
            cluster_config
                .resolve_session_agent("assistant@prod")
                .unwrap(),
            ("assistant".to_string(), "prod".to_string())
        );
    }

    #[test]
    fn resolve_session_agent_rejects_invalid_agent_and_cluster() {
        let config = Config {
            nats_routing: NatsRouting::Cluster("remote".to_string()),
            ..Config::default()
        };
        for agent_ref in [
            "",
            "__temp__",
            harnx_core::agent_config::TEMP_AGENT_NAME,
            "@prod",
        ] {
            assert!(
                config.resolve_session_agent(agent_ref).is_err(),
                "agent ref {agent_ref:?} must be rejected"
            );
        }
        assert!(config.resolve_session_agent("assistant@").is_err());
    }

    #[test]
    fn inspection_arguments_require_agent_and_validate_options() {
        for args in [
            "id",
            "--bogus id",
            "agent id --format",
            "agent id --format yaml --format json",
        ] {
            assert!(
                parse_session_inspection_args(
                    &shell_words::split(args).unwrap(),
                    SessionInspectionCommand::Info
                )
                .is_err(),
                "{args}"
            );
        }
        for args in [
            "reviewer@prod review-12345 --format=json",
            "--format json reviewer@prod review-12345",
        ] {
            assert_eq!(
                parse_session_inspection_args(
                    &shell_words::split(args).unwrap(),
                    SessionInspectionCommand::Info
                )
                .unwrap(),
                (
                    "reviewer@prod".into(),
                    "review-12345".into(),
                    SessionFormat::Json
                )
            );
        }
    }

    fn message_and_tool_entries() -> Vec<SessionLogEntry> {
        vec![
            SessionLogEntry::Message {
                id: Some("message-id".into()),
                role: MessageRole::User,
                content: MessageContent::Text("first line\n---\n\"quoted\" café".into()),
                timestamp: Some("2026-09-12T12:00:00Z".parse().unwrap()),
                fence_token: Some(7),
            },
            SessionLogEntry::ToolCalls {
                text: "reading file".into(),
                thought: Some("check contents\nthen reply".into()),
                calls: vec![ToolCall {
                    name: "read".into(),
                    arguments: json!({"path": "README.md"}),
                    id: Some("call-id".into()),
                    thought_signature: None,
                    reasoning_provenance: None,
                }],
                timestamp: None,
                fence_token: Some(7),
            },
            SessionLogEntry::ToolResults {
                results: vec![ToolOutput {
                    id: Some("call-id".into()),
                    name: "read".into(),
                    output: json!({"content": [{"type": "text", "text": "line 1\nline 2"}]}),
                    markdown: Some("```\nline 1\nline 2\n```".into()),
                    content: vec![],
                    switch_agent: None,
                }],
                timestamp: None,
            },
        ]
    }

    fn entries() -> Vec<SessionLogEntry> {
        let mut entries = message_and_tool_entries();
        entries.extend([
            SessionLogEntry::DataUrls {
                urls: [("cid:attachment".into(), "attachment.png".into())].into(),
            },
            SessionLogEntry::Compress {
                prompt: "keep context\n---\nnext turn".into(),
            },
            SessionLogEntry::Clear,
            SessionLogEntry::EditEntries {
                from: 1,
                to: 1,
                replacements: vec!["type: message\nrole: user\ncontent: replacement\n".into()],
            },
            SessionLogEntry::Rewind { after_seq: 2 },
            SessionLogEntry::Error {
                message: "worker failed".into(),
                fence_token: 7,
                timestamp: None,
            },
            SessionLogEntry::TurnEnd {
                through_seq: 1,
                fence_token: 7,
                timestamp: None,
                usage: None,
            },
            SessionLogEntry::SubAgentStarted {
                agent: "child-agent".into(),
                session_id: "child-session".into(),
                invocation_id: Some("invocation-id".into()),
                tool_call_id: Some("call-id".into()),
                started_at: None,
            },
            SessionLogEntry::HandoffCommitted {
                target_agent: "next-agent".into(),
                target_session_id: "next-session".into(),
                handoff_tool_call_id: Some("call-id".into()),
            },
            SessionLogEntry::HitlApprovalRequested {
                tool_call_id: "call-id".into(),
                summary: "approve read".into(),
                fence_token: 7,
            },
            SessionLogEntry::HitlApprovalDecision {
                tool_call_id: "call-id".into(),
                approved: false,
                note: Some("denied\nby user".into()),
                fence_token: 7,
            },
            SessionLogEntry::Cancel {
                fence_token: 7,
                cancellation_id: None,
                requested_by: None,
                timestamp: None,
            },
            SessionLogEntry::Unknown,
        ]);
        entries
    }

    #[test]
    fn session_format_defaults_to_text() {
        assert_eq!(SessionFormat::default(), SessionFormat::Text);
    }

    #[test]
    fn session_format_parses_case_insensitively_and_displays_canonical_names() -> Result<()> {
        for (format, name) in [
            (SessionFormat::Text, "text"),
            (SessionFormat::Yaml, "yaml"),
            (SessionFormat::Json, "json"),
        ] {
            assert_eq!(name.parse::<SessionFormat>()?, format);
            assert_eq!(name.to_ascii_uppercase().parse::<SessionFormat>()?, format);
            assert_eq!(format.to_string(), name);
        }
        assert_eq!("tExT".parse::<SessionFormat>()?, SessionFormat::Text);
        assert_eq!("yAmL".parse::<SessionFormat>()?, SessionFormat::Yaml);
        assert_eq!("jSoN".parse::<SessionFormat>()?, SessionFormat::Json);
        Ok(())
    }

    #[test]
    fn session_format_rejects_unknown_names() {
        for name in ["", "xml", "jsonl", "yml", " text", "yaml "] {
            let error = name.parse::<SessionFormat>().unwrap_err().to_string();
            assert!(error.contains("expected text, yaml, or json"), "{error}");
        }
    }

    #[test]
    fn entry_emitters_round_trip_all_variants() -> Result<()> {
        for entry in entries() {
            let expected = serde_json::to_value(&entry)?;
            let document = yaml_doc(&entry)?;
            assert!(document.starts_with("---\n"));
            assert!(document.ends_with('\n'));
            assert_eq!(serde_yaml::Deserializer::from_str(&document).count(), 1);
            let yaml_entry: SessionLogEntry = serde_yaml::from_str(&document)?;
            assert_eq!(serde_json::to_value(yaml_entry)?, expected);

            let line = jsonl_line(&entry)?;
            assert!(line.starts_with('{'));
            assert!(line.ends_with("}\n"));
            assert_eq!(line.lines().count(), 1);
            let json_entry: SessionLogEntry = serde_json::from_str(&line)?;
            assert_eq!(serde_json::to_value(json_entry)?, expected);
        }
        Ok(())
    }

    #[test]
    fn dump_entries_yaml_frames_each_document_and_keeps_control_entries() -> Result<()> {
        let entries = entries();
        let dump = dump_entries_yaml(&entries)?;
        assert_eq!(
            dump.lines().filter(|line| *line == "---").count(),
            entries.len()
        );
        assert!(dump.ends_with('\n'));
        let decoded = serde_yaml::Deserializer::from_str(&dump)
            .map(SessionLogEntry::deserialize)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        assert_eq!(
            serde_json::to_value(decoded)?,
            serde_json::to_value(entries)?
        );
        Ok(())
    }

    #[test]
    fn dump_entries_jsonl_emits_one_object_per_line_without_an_array() -> Result<()> {
        let entries = entries();
        let references: Vec<&SessionLogEntry> = entries.iter().collect();
        let dump = dump_entries_jsonl(references)?;
        assert!(dump.starts_with('{'));
        assert!(dump.ends_with("}\n"));
        assert_eq!(dump.lines().count(), entries.len());
        let decoded = dump
            .lines()
            .map(serde_json::from_str::<SessionLogEntry>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        assert_eq!(
            serde_json::to_value(decoded)?,
            serde_json::to_value(entries)?
        );
        Ok(())
    }

    #[test]
    fn empty_batches_emit_nothing() -> Result<()> {
        assert_eq!(dump_entries_yaml([])?, "");
        assert_eq!(dump_entries_jsonl([])?, "");
        Ok(())
    }

    #[test]
    fn batches_and_live_entries_share_framing_regardless_of_sequence_type() -> Result<()> {
        let entries = entries();
        let history: Vec<(usize, _)> = entries.iter().take(8).cloned().enumerate().collect();
        let live: Vec<(u64, _)> = entries
            .iter()
            .skip(8)
            .cloned()
            .enumerate()
            .map(|(seq, entry)| (100 + seq as u64, entry))
            .collect();
        let mut yaml = dump_entries_yaml(history.iter().map(|(_, entry)| entry))?;
        let mut jsonl = dump_entries_jsonl(history.iter().map(|(_, entry)| entry))?;
        for (_, entry) in live {
            yaml.push_str(&yaml_doc(&entry)?);
            jsonl.push_str(&jsonl_line(&entry)?);
        }
        assert_eq!(yaml, dump_entries_yaml(&entries)?);
        assert_eq!(jsonl, dump_entries_jsonl(&entries)?);
        Ok(())
    }

    #[test]
    fn metadata_formats_preserve_full_record_and_large_variables() -> Result<()> {
        let variables = [("file".into(), "line 1\n---\n\"quoted\" café\n".repeat(4096))].into();
        let mut metadata = SessionMetadata::new(
            "session-id",
            SessionInitializer::named("test-agent", variables),
        );
        metadata.overrides.model = Some("openai:gpt-4o".into());
        metadata.title.value = Some("Session title".into());
        metadata.title.manual = true;
        metadata
            .extensions
            .insert("dev.harnx.test".into(), json!({"details": [1, 2]}));

        let yaml = render_metadata_yaml(&metadata)?;
        let yaml_documents = serde_yaml::Deserializer::from_str(&yaml)
            .map(SessionMetadata::deserialize)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        assert_eq!(yaml_documents, vec![metadata.clone()]);

        let json = render_metadata_json(&metadata)?;
        assert_eq!(json.lines().count(), 1);
        assert!(serde_json::from_str::<Value>(&json)?.is_object());
        assert_eq!(serde_json::from_str::<SessionMetadata>(&json)?, metadata);
        Ok(())
    }
}
