use super::*;

use anyhow::{anyhow, Context, Result};
use inquire::{validator::Validation, Text};
use std::{
    fs::{read_dir, read_to_string},
    path::Path,
};

pub use harnx_core::agent_config::{AgentConfig, AgentVariable, AgentVariables, TEMP_AGENT_NAME};

/// Built-in agent name: routes to the title-creation prompt.
pub const CREATE_TITLE_AGENT: &str = harnx_core::agent_config::CREATE_TITLE_AGENT_NAME;

const DEFAULT_AGENT_NAME: &str = "rag";

#[derive(Debug, Clone, Default)]
pub struct Agent {
    config: AgentConfig,
    rag: Option<Arc<Rag>>,
    mcp_manager: Option<Arc<McpManager>>,
}

impl std::ops::Deref for Agent {
    type Target = AgentConfig;
    fn deref(&self) -> &Self::Target {
        &self.config
    }
}

impl std::ops::DerefMut for Agent {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.config
    }
}

impl Agent {
    pub fn new(config: AgentConfig) -> Self {
        Self {
            config,
            rag: None,
            mcp_manager: None,
        }
    }

    pub fn into_config(self) -> AgentConfig {
        self.config
    }

    pub fn rag(&self) -> Option<Arc<Rag>> {
        self.rag.clone()
    }
}

pub fn builtin(name: &str) -> Result<Agent> {
    let content = AgentConfig::builtin_markdown(name)
        .ok_or_else(|| anyhow::anyhow!("Unknown built-in agent `{name}`"))?;
    Ok(Agent::new(AgentConfig::from_markdown(name, content)?))
}

pub fn load(path: &Path) -> Result<Agent> {
    let contents = read_to_string(path)
        .with_context(|| format!("Failed to read agent file at '{}'", path.display()))?;
    let name = path
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("Invalid agent file name: '{}'", path.display()))?;
    Ok(Agent::new(AgentConfig::from_markdown(name, &contents)?))
}

/// Load file-backed defaults for variables that have a `path:` field.
fn resolve_file_backed_variables(variables: &mut [AgentVariable], agent_dir: &Path) -> Result<()> {
    for variable in variables.iter_mut() {
        if let Some(path_str) = &variable.path {
            if variable.default.is_some() {
                log::warn!(
                    "Variable '{}': both 'path' and 'default' set, using 'path'",
                    variable.name
                );
            }

            let resolved_path = safe_join_path(agent_dir, path_str).ok_or_else(|| {
                anyhow!(
                    "Variable '{}': path '{}' is not allowed (must be relative, no '..' traversal)",
                    variable.name,
                    path_str
                )
            })?;

            let content = std::fs::read_to_string(&resolved_path).with_context(|| {
                format!(
                    "Failed to load file '{}' (resolved to '{}') for variable '{}'",
                    path_str,
                    resolved_path.display(),
                    variable.name
                )
            })?;

            variable.default = Some(content);
        }
    }
    Ok(())
}

/// Load file-backed variable defaults onto the agent's variables.
///
/// For each variable with a `path:` field, reads the file and stores its
/// content as the variable's `default`.  This is the subset of init that
/// must run before `init_agent_session_variables` so that user-provided
/// `agent_variables` can still override file defaults.
pub fn resolve_file_defaults(agent: &mut Agent) -> Result<()> {
    let agent_file_path = Config::agent_file(agent.name());
    let agent_dir = agent_file_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(Config::agents_data_dir);
    resolve_file_backed_variables(agent.config.variables_mut(), &agent_dir)
}

/// Resolve file-backed variable defaults and populate `shared_variables`.
///
/// This performs the synchronous subset of `init()` that loads variable
/// values from files (the `path:` field on agent variables) and then
/// runs `init_agent_variables` with `no_interaction: true`.  It does NOT
/// touch MCP, RAG, or model resolution — call `retrieve_agent` for the
/// model and this method for variables when you need a lightweight agent
/// suitable for non-interactive use (e.g. compaction).
pub fn resolve_variables(agent: &mut Agent) -> Result<()> {
    resolve_file_defaults(agent)?;

    let new_variables = init_agent_variables(
        agent.config.defined_variables(),
        agent.config.shared_variables(),
        true, // no_interaction
    )?;
    agent.set_shared_variables(new_variables);
    Ok(())
}

pub async fn init(config: &GlobalConfig, name: &str, abort_signal: AbortSignal) -> Result<Agent> {
    let agent_file_path = Config::agent_file(name);
    let mut agent = if agent_file_path.exists() {
        load(&agent_file_path)?
    } else {
        builtin(name)?
    };

    let mcp_manager = config.read().mcp_manager.clone();
    agent.mcp_manager = mcp_manager.clone();

    let mcp_tools = match &mcp_manager {
        Some(manager) => Some(manager.get_all_tools().await),
        None => None,
    };
    agent.config.set_tools(Tools::init_from_mcp(mcp_tools));

    let model = {
        let config = config.read();
        match agent.model_id() {
            Some(model_id) => {
                crate::client::retrieve_model(&config.clients, model_id, ModelType::Chat)?
            }
            None => {
                if agent.temperature().is_none() {
                    agent.config.set_temperature(config.temperature);
                }
                if agent.top_p().is_none() {
                    agent.config.set_top_p(config.top_p);
                }
                config.current_model().clone()
            }
        }
    };
    agent.config.set_resolved_model(model);

    let rag_path = Config::agent_rag_file(name, DEFAULT_AGENT_NAME);
    let agent_dir = agent_file_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(Config::agents_data_dir);

    resolve_file_backed_variables(agent.config.variables_mut(), &agent_dir)?;

    let rag = if rag_path.exists() {
        Some(Arc::new(Rag::load(
            &config.read().clients,
            DEFAULT_AGENT_NAME,
            &rag_path,
        )?))
    } else if !agent.documents().is_empty() && !config.read().info_flag {
        let mut ans = false;
        if *IS_STDOUT_TERMINAL {
            ans = Confirm::new("The agent has the documents, init RAG?")
                .with_default(true)
                .prompt()?;
        }
        if ans {
            let mut document_paths = vec![];
            for path in agent.documents() {
                if is_url(path) {
                    document_paths.push(path.to_string());
                } else {
                    let new_path = safe_join_path(&agent_dir, path)
                        .ok_or_else(|| anyhow!("Invalid document path: '{path}'"))?;
                    document_paths.push(new_path.display().to_string())
                }
            }
            let (
                clients_owned,
                loaders_owned,
                rag_embedding_model_owned,
                rag_reranker_model,
                rag_top_k,
                rag_chunk_size,
                rag_chunk_overlap,
                user_agent_owned,
                dry_run,
            ) = {
                let cfg = config.read();
                (
                    cfg.clients.clone(),
                    cfg.document_loaders.clone(),
                    cfg.rag_embedding_model.clone(),
                    cfg.rag_reranker_model.clone(),
                    cfg.rag_top_k,
                    cfg.rag_chunk_size,
                    cfg.rag_chunk_overlap,
                    cfg.user_agent.clone(),
                    cfg.dry_run,
                )
            };
            let init_ctx = harnx_rag::RagInitContext {
                clients: &clients_owned,
                document_loaders: &loaders_owned,
                rag_embedding_model: rag_embedding_model_owned.as_deref(),
                rag_reranker_model,
                rag_top_k,
                rag_chunk_size,
                rag_chunk_overlap,
                user_agent: user_agent_owned.as_deref(),
                dry_run,
            };
            let rag = Rag::init(&init_ctx, "rag", &rag_path, &document_paths, abort_signal).await?;
            Some(Arc::new(rag))
        } else {
            None
        }
    } else {
        None
    };
    agent.rag = rag;

    Ok(agent)
}

pub fn init_agent_variables(
    agent_variables: &[AgentVariable],
    variables: &AgentVariables,
    no_interaction: bool,
) -> Result<AgentVariables> {
    let mut output = IndexMap::new();
    if agent_variables.is_empty() {
        return Ok(output);
    }
    let mut printed = false;
    let mut unset_variables = vec![];
    for agent_variable in agent_variables {
        let key = agent_variable.name.clone();
        match variables.get(&key) {
            Some(value) => {
                output.insert(key, value.clone());
            }
            None => {
                if let Some(value) = agent_variable.default.clone() {
                    output.insert(key, value);
                    continue;
                }
                if no_interaction {
                    continue;
                }
                if *IS_STDOUT_TERMINAL {
                    if !printed {
                        crate::utils::emit_info("⚙ Init agent variables...".to_string());
                        printed = true;
                    }
                    let value = Text::new(&format!(
                        "{} ({}):",
                        agent_variable.name, agent_variable.description
                    ))
                    .with_validator(|input: &str| {
                        if input.trim().is_empty() {
                            Ok(Validation::Invalid("This field is required".into()))
                        } else {
                            Ok(Validation::Valid)
                        }
                    })
                    .prompt()?;
                    output.insert(key, value);
                } else {
                    unset_variables.push(agent_variable)
                }
            }
        }
    }
    if !unset_variables.is_empty() {
        bail!(
            "The following agent variables are required:\n{}",
            unset_variables
                .iter()
                .map(|v| format!("  - {}: {}", v.name, v.description))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
    Ok(output)
}

pub fn list_agents() -> Vec<String> {
    let mut output: Vec<String> = match read_dir(Config::agents_data_dir()) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let path = entry.path();
                match path.extension().and_then(|value| value.to_str()) {
                    Some("md") => path
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .map(|value| value.to_string()),
                    _ => None,
                }
            })
            .collect(),
        Err(_) => vec![],
    };
    output.sort();
    output.dedup();
    output
}

pub fn complete_agent_variables(agent_name: &str) -> Vec<(String, Option<String>)> {
    let markdown_path = Config::agents_data_dir().join(format!("{agent_name}.md"));
    if markdown_path.exists() {
        if let Ok(agent) = load(&markdown_path) {
            return agent
                .defined_variables()
                .iter()
                .map(|v| {
                    let description = match &v.default {
                        Some(default) => format!("{} [default: {default}]", v.description),
                        None => v.description.clone(),
                    };
                    (format!("{}=", v.name), Some(description))
                })
                .collect();
        }
    }
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::MessageRole;
    use crate::config::GlobalConfig;
    use crate::utils::create_abort_signal;
    use std::{
        fs,
        path::Path,
        path::PathBuf,
        sync::{LazyLock, Mutex},
        time::{SystemTime, UNIX_EPOCH},
    };

    static TEST_CONFIG_DIR_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn unique_test_config_dir() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "harnx-agent-test-{}-{timestamp}",
            std::process::id()
        ))
    }

    fn with_test_config_dir<T>(f: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
        let _guard = TEST_CONFIG_DIR_LOCK.lock().unwrap();
        let config_dir = unique_test_config_dir();
        let agents_dir = config_dir.join("agents");
        fs::create_dir_all(&agents_dir)?;

        unsafe {
            std::env::set_var("HARNX_CONFIG_DIR", &config_dir);
        }
        let result = f(&config_dir);
        unsafe {
            std::env::remove_var("HARNX_CONFIG_DIR");
        }

        let cleanup_result = fs::remove_dir_all(&config_dir);
        match (result, cleanup_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(err)) => Err(err.into()),
            (Err(err), Ok(())) => Err(err),
            (Err(err), Err(cleanup_err)) => Err(err.context(format!(
                "Additionally failed to clean up test config dir '{}': {cleanup_err}",
                config_dir.display()
            ))),
        }
    }

    fn init_test_agent(agent_name: &str, content: &str, files: &[(&str, &str)]) -> Result<Agent> {
        with_test_config_dir(|config_dir| {
            let agents_dir = config_dir.join("agents");
            fs::write(agents_dir.join(format!("{agent_name}.md")), content)?;

            for (relative_path, file_content) in files {
                let path = agents_dir.join(relative_path);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(path, file_content)?;
            }

            let config = GlobalConfig::default();
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(super::init(&config, agent_name, create_abort_signal()))
        })
    }

    fn make_tool_declaration(name: &str, description: &str) -> crate::tool::ToolDeclaration {
        crate::tool::ToolDeclaration {
            name: name.to_string(),
            description: description.to_string(),
            parameters: Default::default(),
            mcp_tool_name: None,
        }
    }

    fn make_agent_with_tools(prompt: &str, tools: Vec<crate::tool::ToolDeclaration>) -> Agent {
        let mut agent = Agent::new(AgentConfig::from_markdown("test", prompt).unwrap());
        agent
            .config
            .set_tools(crate::tool::Tools::init_from_mcp(if tools.is_empty() {
                None
            } else {
                Some(tools)
            }));
        agent
    }

    #[test]
    fn test_agent_from_markdown_full() {
        let content = "---\nmodel: openai:gpt-4o\ntemperature: 0.7\ntop_p: 0.9\nuse_tools: fs,web_search\ndescription: A test agent\nversion: '1.0'\n---\nYou are a helpful test agent.";
        let agent = AgentConfig::from_markdown("test-agent", content).unwrap();
        assert_eq!(agent.name(), "test-agent");
        assert_eq!(agent.model_id(), Some("openai:gpt-4o"));
        assert_eq!(agent.temperature(), Some(0.7));
        assert_eq!(agent.top_p(), Some(0.9));
        assert_eq!(
            agent.use_tools(),
            Some(vec!["fs".to_string(), "web_search".to_string()])
        );
        assert!(agent
            .interpolated_instructions()
            .contains("You are a helpful test agent"));
    }

    #[test]
    fn test_agent_from_markdown_minimal() {
        let content = "Just instructions, no front-matter.";
        let agent = AgentConfig::from_markdown("minimal", content).unwrap();
        assert_eq!(agent.name(), "minimal");
        assert!(agent.model_id().is_none());
        assert!(agent.temperature().is_none());
        assert_eq!(
            agent.interpolated_instructions(),
            "Just instructions, no front-matter."
        );
    }

    #[test]
    fn test_agent_from_markdown_empty_body() {
        let content = "---\nmodel: openai:gpt-4o\ntemperature: 0.5\n---\n";
        let agent = AgentConfig::from_markdown("empty-body", content).unwrap();
        assert_eq!(agent.name(), "empty-body");
        assert_eq!(agent.model_id(), Some("openai:gpt-4o"));
        assert!(agent.interpolated_instructions().is_empty());
    }

    #[test]
    fn test_agent_set_name() {
        let mut agent = AgentConfig::from_prompt("You are a test agent.");
        assert_eq!(agent.name(), "%%");
        agent.set_name("new-name");
        assert_eq!(agent.name(), "new-name");
    }

    #[test]
    fn test_agent_from_prompt() {
        let agent = AgentConfig::from_prompt("You are a pirate");
        assert_eq!(agent.name(), "%%");
        assert!(agent
            .interpolated_instructions()
            .contains("You are a pirate"));
        assert!(agent.model_id().is_none());
        assert!(agent.temperature().is_none());
    }

    #[test]
    fn test_agent_builtin_create_title() {
        let agent = super::builtin("%create-title%").unwrap();
        assert_eq!(agent.name(), "%create-title%");
        assert!(!agent.interpolated_instructions().is_empty());
        assert!(agent.interpolated_instructions().contains("concise"));
    }

    #[test]
    fn test_agent_builtin_unknown() {
        let result = super::builtin("unknown-agent");
        assert!(result.is_err());
    }

    #[test]
    fn test_agent_from_markdown_with_use_tools() {
        let content = "---\nuse_tools: fs_*,bash_exec\n---\nHelp with files.";
        let agent = AgentConfig::from_markdown("tools-agent", content).unwrap();
        assert_eq!(
            agent.use_tools(),
            Some(vec!["fs_*".to_string(), "bash_exec".to_string()])
        );
    }

    #[test]
    fn test_agent_compaction_agent_set() {
        let content = "---\ncompaction_agent: my-compactor\n---\nYou are a test agent.";
        let agent = AgentConfig::from_markdown("test-agent", content).unwrap();
        assert_eq!(agent.compaction_agent(), Some("my-compactor"));
    }

    #[test]
    fn test_agent_compaction_agent_unset() {
        let content = "---\nmodel: openai:gpt-4o\n---\nYou are a test agent.";
        let agent = AgentConfig::from_markdown("test-agent", content).unwrap();
        assert!(agent.compaction_agent().is_none());
    }

    #[test]
    fn test_agent_compaction_agent_roundtrip() {
        let content =
            "---\ncompaction_agent: my-compactor\nmodel: openai:gpt-4o\n---\nYou are a test agent.";
        let agent = AgentConfig::from_markdown("test-agent", content).unwrap();

        // Export and re-parse
        let exported = agent.export().unwrap();
        let reparsed = AgentConfig::from_markdown("test-agent", &exported).unwrap();

        assert_eq!(reparsed.compaction_agent(), Some("my-compactor"));
        assert_eq!(reparsed.model_id(), Some("openai:gpt-4o"));
    }

    #[test]
    fn test_tools_text_with_tools() {
        let agent = make_agent_with_tools(
            "prompt",
            vec![
                make_tool_declaration("tool_a", "Description A"),
                make_tool_declaration("tool_b", "Description B"),
            ],
        );

        let text = agent.tools_text();

        assert_eq!(
            text,
            Some("1. tool_a: Description A\n2. tool_b: Description B".to_string())
        );
    }

    #[test]
    fn test_tools_text_without_tools() {
        let agent = make_agent_with_tools("prompt", vec![]);

        assert_eq!(agent.tools_text(), None);
    }

    #[test]
    fn test_tools_text_multiline_description() {
        let agent = make_agent_with_tools(
            "prompt",
            vec![make_tool_declaration(
                "tool_x",
                "First line\nSecond line\nThird line",
            )],
        );

        let text = agent.tools_text();

        assert_eq!(text, Some("1. tool_x: First line".to_string()));
    }

    #[test]
    fn test_export_does_not_contain_tool_text() {
        let agent = make_agent_with_tools(
            "You are a helpful assistant.",
            vec![make_tool_declaration("my_tool", "Tool description")],
        );

        let exported = agent.export().unwrap();

        assert!(!exported.contains("my_tool"));
        assert!(!exported.contains("Tool description"));
        assert!(exported.contains("You are a helpful assistant."));
    }

    #[test]
    fn test_build_messages_always_uses_system_and_user_format() {
        let config = GlobalConfig::default();
        let agent = Agent::new(AgentConfig::from_prompt(
            "System message\n__INPUT__\n\n### INPUT:\nExample input\n### OUTPUT:\nExample output",
        ));
        let input = crate::config::input::from_str(&config, "Real input", Some(agent));

        let messages = input.agent().build_messages(&input);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, MessageRole::System);
        assert_eq!(messages[1].role, MessageRole::User);
        assert_eq!(
            messages[0].content.to_text(),
            "System message\n__INPUT__\n\n### INPUT:\nExample input\n### OUTPUT:\nExample output"
        );
        assert_eq!(messages[1].content.to_text(), "Real input");
    }

    #[test]
    fn test_agent_variable_path_deserialization() {
        let yaml = r#"name: prompt
description: Shared prompt
path: shared/prompt.md
"#;

        let variable: AgentVariable = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(variable.name, "prompt");
        assert_eq!(variable.description, "Shared prompt");
        assert_eq!(variable.path.as_deref(), Some("shared/prompt.md"));
        assert!(variable.default.is_none());
        assert!(variable.value.is_empty());
    }

    #[test]
    fn test_agent_variable_path_serialization() {
        let variable = AgentVariable {
            name: "prompt".to_string(),
            description: "Shared prompt".to_string(),
            default: None,
            path: Some("shared/prompt.md".to_string()),
            value: "runtime-only".to_string(),
        };

        let yaml = serde_yaml::to_string(&variable).unwrap();
        let round_trip: AgentVariable = serde_yaml::from_str(&yaml).unwrap();

        assert!(yaml.contains("path: shared/prompt.md"));
        assert!(!yaml.contains("value:"));
        assert_eq!(round_trip.name, "prompt");
        assert_eq!(round_trip.description, "Shared prompt");
        assert_eq!(round_trip.path.as_deref(), Some("shared/prompt.md"));
        assert!(round_trip.default.is_none());
        assert!(round_trip.value.is_empty());
    }

    #[test]
    fn test_agent_variable_without_path() {
        let yaml = r#"name: prompt
description: Shared prompt
"#;

        let variable: AgentVariable = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(variable.name, "prompt");
        assert_eq!(variable.description, "Shared prompt");
        assert!(variable.path.is_none());
        assert!(variable.default.is_none());
        assert!(variable.value.is_empty());
    }

    #[test]
    fn test_agent_variable_with_path() {
        let agent = init_test_agent(
            "path-variable",
            r#"---
variables:
  - name: prompt
    description: Shared prompt
    path: shared/prompt.md
---
You are a test agent.
"#,
            &[("shared/prompt.md", "Loaded prompt")],
        )
        .unwrap();

        assert_eq!(
            agent.defined_variables()[0].default.as_deref(),
            Some("Loaded prompt")
        );
    }

    #[test]
    fn test_agent_variable_path_missing_file() {
        let error = init_test_agent(
            "missing-path-variable",
            r#"---
variables:
  - name: prompt
    description: Shared prompt
    path: shared/missing.md
---
You are a test agent.
"#,
            &[],
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("prompt"));
        assert!(message.contains("shared/missing.md"));
    }

    #[test]
    fn test_agent_variable_path_traversal_rejected() {
        let error = init_test_agent(
            "traversal-path-variable",
            r#"---
variables:
  - name: prompt
    description: Shared prompt
    path: ../../../etc/passwd
---
You are a test agent.
"#,
            &[],
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("prompt"));
        assert!(message.contains("../../../etc/passwd"));
        assert!(message.contains("not allowed"));
    }

    #[test]
    fn test_agent_variable_path_absolute_rejected() {
        let error = init_test_agent(
            "absolute-path-variable",
            r#"---
variables:
  - name: prompt
    description: Shared prompt
    path: /etc/passwd
---
You are a test agent.
"#,
            &[],
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("prompt"));
        assert!(message.contains("/etc/passwd"));
        assert!(message.contains("not allowed"));
    }

    #[test]
    fn test_agent_variable_path_empty_file() {
        let agent = init_test_agent(
            "empty-path-variable",
            r#"---
variables:
  - name: prompt
    description: Shared prompt
    path: shared/empty.md
---
You are a test agent.
"#,
            &[("shared/empty.md", "")],
        )
        .unwrap();

        assert_eq!(agent.defined_variables()[0].default.as_deref(), Some(""));
    }

    #[test]
    fn test_agent_variable_path_and_default_uses_path() {
        let agent = init_test_agent(
            "path-and-default-variable",
            r#"---
variables:
  - name: prompt
    description: Shared prompt
    default: Inline prompt
    path: shared/prompt.md
---
You are a test agent.
"#,
            &[("shared/prompt.md", "Loaded from file")],
        )
        .unwrap();

        assert_eq!(
            agent.defined_variables()[0].default.as_deref(),
            Some("Loaded from file")
        );
    }

    #[test]
    fn test_agent_variable_path_nested_relative_file() {
        let agent = init_test_agent(
            "nested-relative-path-variable",
            r#"---
variables:
  - name: prompt
    description: Shared prompt
    path: shared/nested/prompt.md
---
You are a test agent.
"#,
            &[("shared/nested/prompt.md", "Nested prompt")],
        )
        .unwrap();

        assert_eq!(
            agent.defined_variables()[0].default.as_deref(),
            Some("Nested prompt")
        );
    }
}
