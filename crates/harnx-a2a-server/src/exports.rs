//! Export registry: resolve CLI agent specs to validated exports with lookup keys.
//!
//! At startup, each `--agent` SPEC is validated and resolved to an [`Export`]
//! containing public name, agent name, cluster, and Agent Card metadata.
//! Lookup keys are registered for URL routing.

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use harnx_core::{agent_ref::AgentRef, package_namespace::sanitize_for_tool_name};
use harnx_runtime::config::{AgentConfig, Config, LOCAL_CLUSTER_KEY};

use crate::cli::AgentSpec;

/// Wildcard/glob characters rejected in agent names.
const GLOB_CHARS: &[char] = &['*', '?', '[', ']', '{', '}'];

/// Metadata for the Agent Card, extracted from `AgentConfig`.
#[derive(Clone, Debug)]
pub struct AgentCardMeta {
    pub name: String,
    pub description: String,
    pub version: String,
    pub conversation_starters: Vec<String>,
}

/// A resolved agent export with all lookup keys registered.
#[derive(Clone, Debug)]
pub struct Export {
    /// The public URL name (alias if set, else sanitized form).
    pub public_name: String,
    /// The agent name (package-qualified, WITHOUT `@cluster`).
    pub agent: String,
    /// The cluster name, if remote.
    pub cluster: Option<String>,
    /// Metadata for the Agent Card.
    pub card_meta: AgentCardMeta,
    /// All lookup keys for this export:
    /// - exact name (after percent-decoding)
    /// - sanitized form (`pkg__agent`)
    /// - explicit alias if given
    pub lookup_keys: Vec<String>,
}

impl Export {
    /// Build the agent reference for session initialization and activation.
    ///
    /// Returns `agent@cluster` if a cluster is set, otherwise the bare agent name.
    pub fn agent_ref(&self) -> String {
        match &self.cluster {
            Some(c) => format!("{}@{}", self.agent, c),
            None => self.agent.clone(),
        }
    }
}

/// Validate and resolve all agent specs into exports with registered lookup keys.
///
/// # Errors
///
/// Returns an error if:
/// - Any spec contains wildcard/glob characters
/// - Any agent does not exist
/// - Any lookup key collides with a previously registered export
/// - Path safety check fails (traversal, unsafe characters)
pub async fn resolve_exports(
    specs: &[AgentSpec],
    cluster: Option<&str>,
    config_dir: Option<&std::path::Path>,
) -> Result<Vec<Export>> {
    // Runtime path helpers read HARNX_CONFIG_DIR, including package paths. Keep
    // the override alive through resolution, with no await while it is active.
    let _config_dir = ConfigDirOverride::new(config_dir);
    let config_path = config_dir
        .map(|dir| dir.join("config.yaml"))
        .unwrap_or_else(Config::config_file);
    let mut config = Config::load_from_file(&config_path)
        .with_context(|| format!("failed to load config from {}", config_path.display()))?;
    config.apply_frontend_nats_routing();

    let mut exports = Vec::with_capacity(specs.len());
    let mut registry: HashMap<String, String> = HashMap::new(); // lookup_key -> public_name

    for spec in specs {
        let export = resolve_export(spec, cluster, &config)?;

        for key in &export.lookup_keys {
            if let Some(existing) = registry.get(key) {
                bail!(
                    "export collision: lookup key '{}' claimed by both '{}' and '{}'",
                    key,
                    existing,
                    export.public_name
                );
            }
            registry.insert(key.clone(), export.public_name.clone());
        }

        exports.push(export);
    }

    Ok(exports)
}

/// Scoped startup override for the runtime's process-wide path helpers.
struct ConfigDirOverride(Option<Option<std::ffi::OsString>>);

impl ConfigDirOverride {
    fn new(dir: Option<&std::path::Path>) -> Self {
        Self(dir.map(|dir| {
            let previous = std::env::var_os("HARNX_CONFIG_DIR");
            std::env::set_var("HARNX_CONFIG_DIR", dir);
            previous
        }))
    }
}

impl Drop for ConfigDirOverride {
    fn drop(&mut self) {
        if let Some(previous) = &self.0 {
            match previous {
                Some(value) => std::env::set_var("HARNX_CONFIG_DIR", value),
                None => std::env::remove_var("HARNX_CONFIG_DIR"),
            }
        }
    }
}

struct ParsedAgent {
    name: String,
    cluster: Option<String>,
}

fn parse_agent_and_cluster(
    spec: &AgentSpec,
    cluster: Option<&str>,
    config: &Config,
) -> Result<ParsedAgent> {
    let name = &spec.name;
    if name.contains(GLOB_CHARS) {
        bail!(
            "agent name '{}' contains wildcard characters, which are not allowed",
            name
        );
    }

    let (agent, selected_cluster) = match AgentRef::parse(name) {
        AgentRef::Remote { agent, cluster } => (agent.into_owned(), Some(cluster.into_owned())),
        AgentRef::Local(agent) => (
            agent.into_owned(),
            cluster
                .map(str::to_owned)
                .or_else(|| config.default_cluster_for_display().map(str::to_owned)),
        ),
    };
    Ok(ParsedAgent {
        name: agent,
        cluster: selected_cluster,
    })
}

fn validate_alias(spec: &AgentSpec) -> Result<()> {
    let Some(alias) = &spec.alias else {
        return Ok(());
    };
    if !is_safe_path_segment(alias) || alias.contains(GLOB_CHARS) {
        bail!("agent alias '{alias}' is invalid (expected a safe URL path segment)");
    }
    Ok(())
}

fn validate_cluster(cluster: &str, config: &Config) -> Result<()> {
    if cluster.trim().is_empty() || cluster.contains(GLOB_CHARS) {
        bail!("cluster name '{cluster}' is invalid");
    }
    if cluster != LOCAL_CLUSTER_KEY {
        config.nats_server(cluster)?;
    }
    Ok(())
}

fn load_agent_card_meta(config: &Config, parsed: &ParsedAgent) -> Result<AgentCardMeta> {
    let agent = &parsed.name;
    if !Config::agent_file(agent).exists() && AgentConfig::builtin_markdown(agent).is_none() {
        bail!("agent '{agent}' not found");
    }
    let scoped = config.fork_session_scope();
    let agent_config = scoped
        .retrieve_agent(agent)
        .with_context(|| format!("failed to load agent '{agent}'"))?;

    Ok(AgentCardMeta {
        name: agent_config.name().to_string(),
        description: agent_config.description().to_string(),
        version: agent_config.version().to_string(),
        conversation_starters: agent_config.conversation_staters().to_vec(),
    })
}

fn compute_lookup_keys(parsed: &ParsedAgent, sanitized: &str, spec: &AgentSpec) -> Vec<String> {
    let mut lookup_keys = vec![parsed.name.clone(), sanitized.to_owned()];
    if let Some(alias) = &spec.alias {
        lookup_keys.push(alias.to_owned());
    }
    lookup_keys.sort();
    lookup_keys.dedup();
    lookup_keys
}

/// Resolve a single agent spec to an export.
fn resolve_export(spec: &AgentSpec, cluster: Option<&str>, config: &Config) -> Result<Export> {
    let parsed = parse_agent_and_cluster(spec, cluster, config)?;
    validate_agent_name(&parsed)?;
    validate_alias(spec)?;
    if let Some(cluster) = &parsed.cluster {
        validate_cluster(cluster, config)?;
    }

    let card_meta = load_agent_card_meta(config, &parsed)?;
    let sanitized = sanitize_for_tool_name(&parsed.name);
    let public_name = spec.alias.clone().unwrap_or_else(|| sanitized.clone());
    let lookup_keys = compute_lookup_keys(&parsed, &sanitized, spec);

    Ok(Export {
        public_name,
        agent: parsed.name,
        cluster: parsed.cluster,
        card_meta,
        lookup_keys,
    })
}

/// Validate agent name for path safety (no traversal, safe segments).
fn validate_agent_name(parsed: &ParsedAgent) -> Result<()> {
    let name = &parsed.name;
    if !is_safe_agent_path(name) {
        bail!(
            "agent name '{}' is invalid (path traversal or unsafe characters)",
            name
        );
    }
    Ok(())
}

/// Check if an agent path is safe (from harnx-serve/agent_resolve.rs).
fn is_safe_agent_path(value: &str) -> bool {
    !value.is_empty() && !value.starts_with('/') && value.split('/').all(is_safe_path_segment)
}

/// Check if a path segment is safe.
fn is_safe_path_segment(value: &str) -> bool {
    !value.is_empty()
        && std::path::Path::new(value)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        && !value.contains(['/', '\\'])
}

/// Lookup an export by name (URL path segment after `/agents/`).
///
/// The name may be:
/// - The exact agent name (percent-decoded by axum, e.g., `pkg/agent` from `pkg%2Fagent`)
/// - The sanitized form (`pkg__agent`)
/// - An explicit alias
///
/// Returns `None` if no export matches.
pub fn lookup_export<'a>(exports: &'a [Export], name: &str) -> Option<&'a Export> {
    exports
        .iter()
        .find(|e| e.lookup_keys.iter().any(|k| k == name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{EnvGuard, TestConfigSandbox as TestSandbox};

    const UNSAFE_PATHS: &[&str] = &[
        "",
        "/agent",
        "../agent",
        "pkg/../agent",
        "agent/",
        "a\\b",
        "../agent@shared",
        "pkg/../agent@shared",
    ];

    const UNSAFE_ALIASES: &[&str] = &["", ".", "..", "a/b", "a\\b", "*"];

    struct AgentFixture<'a> {
        name: &'a str,
        description: &'a str,
        prompt: &'a str,
    }

    struct FrontMatterFixture<'a> {
        name: &'a str,
        front_matter: &'a str,
        prompt: &'a str,
    }

    struct TestFixture {
        sandbox: TestSandbox,
    }

    impl TestFixture {
        fn new() -> Self {
            Self {
                sandbox: TestSandbox::new(),
            }
        }

        fn with_agent(self, agent: AgentFixture<'_>) -> Self {
            self.sandbox
                .write_agent(agent.name, agent.description, agent.prompt);
            self
        }

        fn with_simple_agent(self, name: &str) -> Self {
            self.with_agent(AgentFixture {
                name,
                description: "Test agent",
                prompt: "You are helpful.",
            })
        }

        fn with_front_matter(self, agent: FrontMatterFixture<'_>) -> Self {
            self.sandbox.write_agent_with_front_matter(
                agent.name,
                agent.front_matter,
                agent.prompt,
            );
            self
        }

        fn with_cluster(self, name: &str) -> Self {
            self.sandbox.write_cluster(name);
            self
        }

        fn config_dir(&self) -> &std::path::Path {
            self.sandbox.config_dir()
        }

        async fn resolve(&self, specs: &[AgentSpec]) -> Result<Vec<Export>> {
            resolve_exports(specs, None, Some(self.config_dir())).await
        }

        async fn resolve_cluster(&self, specs: &[AgentSpec], cluster: &str) -> Result<Vec<Export>> {
            resolve_exports(specs, Some(cluster), Some(self.config_dir())).await
        }
    }

    fn spec(name: &str) -> AgentSpec {
        AgentSpec {
            alias: None,
            name: name.to_string(),
        }
    }

    fn spec_with_alias(alias: &str, name: &str) -> AgentSpec {
        AgentSpec {
            alias: Some(alias.to_string()),
            name: name.to_string(),
        }
    }

    fn assert_lookup_keys(exports: &[Export], names: &[&str]) {
        let export = &exports[0];
        for name in names {
            assert!(
                export.lookup_keys.iter().any(|key| key == name),
                "missing key {name}"
            );
            assert!(
                lookup_export(exports, name).is_some(),
                "unreachable key {name}"
            );
        }
    }

    #[tokio::test]
    async fn export_pkg_agent_slash_reachable_as_encoded_and_sanitized() {
        let fixture = TestFixture::new().with_simple_agent("pkg/agent");
        let exports = fixture.resolve(&[spec("pkg/agent")]).await.unwrap();
        assert_eq!(exports.len(), 1);

        let e = &exports[0];
        assert_eq!(
            (
                e.public_name.as_str(),
                e.agent.as_str(),
                e.cluster.as_deref()
            ),
            ("pkg__agent", "pkg/agent", None)
        );
        assert_lookup_keys(&exports, &["pkg/agent", "pkg__agent"]);
    }

    #[tokio::test]
    async fn export_agent_with_literal_double_underscore_resolves() {
        let fixture = TestFixture::new().with_agent(AgentFixture {
            name: "a__b",
            description: "Underscore agent",
            prompt: "You use underscores.",
        });
        let exports = fixture.resolve(&[spec("a__b")]).await.unwrap();
        assert_eq!(exports.len(), 1);

        let e = &exports[0];
        assert_eq!((e.public_name.as_str(), e.agent.as_str()), ("a__b", "a__b"));

        assert!(lookup_export(&exports, "a__b").is_some());
    }

    #[tokio::test]
    async fn export_collisions_fail_startup() {
        let fixture = TestFixture::new()
            .with_simple_agent("pkg/agent")
            .with_simple_agent("pkg__agent")
            .with_simple_agent("agent1")
            .with_simple_agent("foo");
        let collisions = [
            [spec("pkg/agent"), spec("pkg__agent")],
            [spec_with_alias("foo", "agent1"), spec("foo")],
            [spec("agent1"), spec("agent1")],
        ];
        for specs in collisions {
            let error = fixture.resolve(&specs).await.unwrap_err();
            assert!(
                error.to_string().contains("collision"),
                "{specs:?}: {error:#}"
            );
        }
    }

    #[tokio::test]
    async fn export_path_traversal_rejected() {
        let fixture = TestFixture::new();
        let result = fixture.resolve(&[spec("../etc/passwd")]).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("invalid") || err.contains("not found"),
            "error: {}",
            err
        );
    }

    #[tokio::test]
    async fn export_wildcard_rejected() {
        let fixture = TestFixture::new();
        let result = fixture.resolve(&[spec("agent*")]).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("wildcard"), "error: {}", err);
    }

    #[tokio::test]
    async fn export_glob_brace_rejected() {
        let fixture = TestFixture::new();
        let result = fixture.resolve(&[spec("agent/{foo,bar}")]).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("wildcard"), "error: {}", err);
    }

    #[tokio::test]
    async fn export_unknown_agent_fails_startup() {
        let fixture = TestFixture::new();
        let result = fixture.resolve(&[spec("nonexistent")]).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not found"), "error: {}", err);
    }

    #[tokio::test]
    async fn export_unknown_route_returns_none() {
        let fixture = TestFixture::new().with_agent(AgentFixture {
            name: "agent1",
            description: "Test",
            prompt: "You are helpful.",
        });
        let exports = fixture.resolve(&[spec("agent1")]).await.unwrap();
        assert!(lookup_export(&exports, "unknown").is_none());
    }

    #[tokio::test]
    async fn export_with_cluster() {
        let fixture = TestFixture::new()
            .with_agent(AgentFixture {
                name: "pkg/agent",
                description: "Test",
                prompt: "You are helpful.",
            })
            .with_cluster("shared");

        let exports = fixture
            .resolve_cluster(&[spec("pkg/agent")], "shared")
            .await
            .unwrap();
        let e = &exports[0];
        assert_eq!(e.agent, "pkg/agent");
        assert_eq!(e.cluster, Some("shared".to_string()));
        assert_eq!(e.agent_ref(), "pkg/agent@shared");
    }

    #[tokio::test]
    async fn export_alias_used_as_public_name() {
        let fixture = TestFixture::new().with_agent(AgentFixture {
            name: "pkg/agent",
            description: "Test",
            prompt: "You are helpful.",
        });
        let exports = fixture
            .resolve(&[spec_with_alias("my-tool", "pkg/agent")])
            .await
            .unwrap();
        let e = &exports[0];
        assert_eq!(e.public_name, "my-tool");
        assert_lookup_keys(&exports, &["my-tool", "pkg/agent", "pkg__agent"]);
    }

    #[tokio::test]
    async fn export_sanitized_key_not_reverse_resolved() {
        let fixture = TestFixture::new().with_agent(AgentFixture {
            name: "pkg__agent",
            description: "Underscore agent",
            prompt: "You use underscores.",
        });
        let exports = fixture.resolve(&[spec("pkg__agent")]).await.unwrap();
        let e = &exports[0];
        assert_eq!(e.public_name, "pkg__agent");
        assert!(lookup_export(&exports, "pkg__agent").is_some());
        assert!(lookup_export(&exports, "pkg/agent").is_none());
    }

    #[tokio::test]
    async fn export_sanitized_key_unique_from_slash_name() {
        let fixture = TestFixture::new()
            .with_agent(AgentFixture {
                name: "pkg/agent",
                description: "Slash agent",
                prompt: "You use slashes.",
            })
            .with_agent(AgentFixture {
                name: "a__b",
                description: "Underscore agent",
                prompt: "You use underscores.",
            });

        let exports = fixture
            .resolve(&[spec("pkg/agent"), spec("a__b")])
            .await
            .unwrap();
        assert_eq!(exports.len(), 2);

        let e1 = lookup_export(&exports, "pkg__agent").unwrap();
        assert_eq!(e1.agent, "pkg/agent");

        let e2 = lookup_export(&exports, "a__b").unwrap();
        assert_eq!(e2.agent, "a__b");
    }

    #[tokio::test]
    async fn export_card_meta_populated() {
        let fixture = TestFixture::new().with_front_matter(FrontMatterFixture { name: "myagent", front_matter: "description: A test agent for card metadata\nversion: 1.2.3\nconversation_starters:\n  - Say hello\n  - Help me plan", prompt: "You are helpful." });

        let exports = fixture.resolve(&[spec("myagent")]).await.unwrap();
        let e = &exports[0];
        assert_eq!(
            (
                e.card_meta.name.as_str(),
                e.card_meta.description.as_str(),
                e.card_meta.version.as_str(),
                e.card_meta.conversation_starters.as_slice()
            ),
            (
                "myagent",
                "A test agent for card metadata",
                "1.2.3",
                ["Say hello".to_owned(), "Help me plan".to_owned()].as_slice()
            ),
        );
    }

    #[tokio::test]
    async fn export_local_agent_cannot_have_at_cluster() {
        let fixture = TestFixture::new().with_agent(AgentFixture {
            name: "agent",
            description: "Test",
            prompt: "You are helpful.",
        });
        let result = fixture.resolve(&[spec("agent@cluster")]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn export_rejects_unsafe_bare_and_remote_paths() {
        let fixture = TestFixture::new();
        for name in UNSAFE_PATHS {
            let err = fixture.resolve(&[spec(name)]).await.unwrap_err();
            assert!(err.to_string().contains("invalid"), "{name}: {err:#}");
        }
    }

    #[tokio::test]
    async fn export_rejects_unsafe_aliases() {
        let fixture = TestFixture::new().with_agent(AgentFixture {
            name: "agent",
            description: "Test",
            prompt: "Prompt",
        });
        for alias in UNSAFE_ALIASES {
            let err = fixture
                .resolve(&[spec_with_alias(alias, "agent")])
                .await
                .unwrap_err();
            assert!(err.to_string().contains("alias"), "{alias}: {err:#}");
        }
    }

    #[tokio::test]
    async fn export_config_dir_overrides_env_and_restores_on_error() {
        let fixture = TestFixture::new().with_agent(AgentFixture {
            name: "agent",
            description: "Selected directory",
            prompt: "Prompt",
        });
        let other = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("HARNX_CONFIG_DIR", Some(other.path().as_os_str()));
        let before = std::env::var_os("HARNX_CONFIG_DIR");
        let exports = fixture.resolve(&[spec("agent")]).await.unwrap();
        assert_eq!(exports[0].card_meta.description, "Selected directory");
        assert_eq!(std::env::var_os("HARNX_CONFIG_DIR"), before);
        fixture.resolve(&[spec("missing")]).await.unwrap_err();
        assert_eq!(std::env::var_os("HARNX_CONFIG_DIR"), before);
    }

    #[tokio::test]
    async fn export_uses_config_dir_env_without_cli_override() {
        let _fixture = TestFixture::new().with_agent(AgentFixture {
            name: "agent",
            description: "From environment",
            prompt: "Prompt",
        });
        let exports = resolve_exports(&[spec("agent")], None, None).await.unwrap();
        assert_eq!(exports[0].card_meta.description, "From environment");
    }

    #[tokio::test]
    async fn export_explicit_cluster_is_separate_and_wins_over_cli() {
        let fixture = TestFixture::new()
            .with_agent(AgentFixture {
                name: "pkg/agent",
                description: "Test",
                prompt: "Prompt",
            })
            .with_cluster("shared");
        let exports = fixture
            .resolve_cluster(&[spec("pkg/agent@shared")], "ignored")
            .await
            .unwrap();
        assert_eq!(
            (exports[0].agent.as_str(), exports[0].cluster.as_deref()),
            ("pkg/agent", Some("shared"))
        );
        assert_eq!(exports[0].agent_ref(), "pkg/agent@shared");
        assert!(lookup_export(&exports, "pkg/agent@shared").is_none());
    }
}
