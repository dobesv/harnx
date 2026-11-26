//! Every agent loaded the way a session loads it and rendered the way
//! `harnx info agent` renders it, since agents otherwise load only when
//! first used; then the models agents and `config.yaml` name.

use std::path::{Path, PathBuf};

use anyhow::Result;
use harnx_core::agent_config::{AgentConfig, AgentVariables};
use harnx_core::package_namespace::pkg_from_qualified;

use super::{
    names_a_file_with_errors, without_root, CheckContext, Code, ConfigLayout, Finding, Report,
};
use crate::client::{
    install_models_override, list_all_models, list_client_names, retrieve_model, ClientConfig,
    ModelType,
};
use crate::config::agent::{list_agents, load_with_qualified_name, render_agent_dump};
use crate::config::Config;

/// An agent that loaded, with its config after the package patch and the
/// package-relative model rewriting.
#[derive(Debug, Clone)]
pub struct LoadedAgent {
    /// `name` for a local agent, `package/name` for a package agent.
    pub name: String,
    pub file: PathBuf,
    pub package: Option<String>,
    pub config: AgentConfig,
}

/// Load and render every local and package agent, reporting each that
/// fails, then check the models they and `config.yaml` name. `config` is the
/// loaded directory, `None` when it doesn't load: agents are then rendered
/// against an empty config and models aren't checked, since there are no
/// clients to resolve them against. `config` is borrowed mutably only to
/// lend each render its variables, and is left as it was.
pub fn check_agents(
    context: &CheckContext<'_>,
    config: Option<&mut Config>,
    report: &mut Report,
) -> Vec<LoadedAgent> {
    let layout = context.layout;
    let loaded = config.is_some();
    let mut empty = Config::default();
    let config = config.unwrap_or(&mut empty);
    let agents: Vec<LoadedAgent> = list_agents()
        .into_iter()
        .filter(|name| !name.contains('@'))
        .filter_map(|name| load_agent(layout, config, name, report))
        .collect();
    if loaded {
        let models = ModelCheck {
            layout,
            clients: &config.clients,
            package_mode: context.options.package_mode,
        };
        models.check(config, &agents, report);
    }
    agents
}

/// Load `name` as a session would, then render it with its declared
/// variables. A render failure still returns the agent: its config loaded.
fn load_agent(
    layout: &ConfigLayout,
    config: &mut Config,
    name: String,
    report: &mut Report,
) -> Option<LoadedAgent> {
    let file = Config::agent_file(&name);
    let agent = load_with_qualified_name(&file, &name)
        .inspect_err(|error| report_load_error(layout, &name, &file, error, report))
        .ok()?;
    let _prompt = render_with_variables(config, &agent, &name)
        .inspect_err(|error| report_load_error(layout, &name, &file, error, report));
    Some(LoadedAgent {
        package: pkg_from_qualified(&name).map(str::to_string),
        name,
        file,
        config: agent.into_config(),
    })
}

/// Render `name` with a value for every variable it declares. One with
/// neither a default nor a file is given at runtime, on the command line,
/// by the session or at a prompt, so it gets a placeholder here; the
/// render fills in the rest itself.
fn render_with_variables(config: &mut Config, agent: &AgentConfig, name: &str) -> Result<String> {
    let variables = supplied_variables(config.agent_variables.clone(), agent);
    let saved = config.agent_variables.replace(variables);
    let rendered = render_agent_dump(config, name);
    config.agent_variables = saved;
    rendered
}

/// `variables`, plus a placeholder for each variable `agent` declares that
/// has neither a default nor a file and no value in `variables`.
fn supplied_variables(variables: Option<AgentVariables>, agent: &AgentConfig) -> AgentVariables {
    let mut variables = variables.unwrap_or_default();
    let given_at_runtime = agent
        .defined_variables()
        .iter()
        .filter(|variable| variable.default.is_none() && variable.path.is_none());
    for variable in given_at_runtime {
        variables
            .entry(variable.name.clone())
            .or_insert_with(|| format!("<{}>", variable.name));
    }
    variables
}

/// An `agent-load` error on the agent's file, unless a file the error comes
/// from already has a parse error saying the same.
fn report_load_error(
    layout: &ConfigLayout,
    name: &str,
    file: &Path,
    error: &anyhow::Error,
    report: &mut Report,
) {
    let file = layout.relative(file);
    let message = without_root(
        &format!("agent '{name}' does not load: {error:#}"),
        &layout.root,
    );
    if has_parse_error(report, &file) || names_a_file_with_errors(&message, report) {
        return;
    }
    report.push(Finding::of(Code::AgentLoad, message).in_file(file));
}

fn has_parse_error(report: &Report, file: &Path) -> bool {
    report
        .findings
        .iter()
        .any(|finding| finding.code == Code::ParseError && finding.file.as_deref() == Some(file))
}

/// What the model check needs: where findings go, the clients models resolve
/// against, and whether the directory holds only packages
/// ([`super::CheckOptions::package_mode`]).
struct ModelCheck<'a> {
    layout: &'a ConfigLayout,
    clients: &'a [ClientConfig],
    package_mode: bool,
}

impl ModelCheck<'_> {
    /// `config.yaml`'s `model`, then each agent's `model` and
    /// `model_fallbacks`.
    fn check(&self, config: &Config, agents: &[LoadedAgent], report: &mut Report) {
        // The catalog is read once, on first use: the override has to be in
        // place before any model lookup.
        install_models_override();
        if let Some(file) = &self.layout.config_file {
            let model = &config.data.model_id;
            if !model.is_empty() {
                let file = self.layout.relative(file);
                self.check_model("model", model, None, &file, report);
            }
        }
        for agent in agents {
            let file = self.layout.relative(&agent.file);
            let package = agent.package.as_deref();
            for (key, id) in agent_models(&agent.config) {
                self.check_model(key, id, package, &file, report);
            }
        }
    }

    /// A model that names no configured client is an error; one the
    /// client's catalog lacks is a warning, since harnx accepts any chat
    /// model name on a known client.
    fn check_model(
        &self,
        key: &'static str,
        id: &str,
        package: Option<&str>,
        file: &Path,
        report: &mut Report,
    ) {
        let clients = self.clients;
        let client = id.split_once(':').map_or(id, |(client, _)| client);
        let finding = match retrieve_model(clients, id, ModelType::Chat) {
            Err(error) => Some(self.unresolved(key, id, client, package, &error)),
            Ok(model)
                if !list_all_models(clients)
                    .iter()
                    .any(|m| m.id() == model.id()) =>
            {
                Some(not_in_catalog(key, id, client))
            }
            Ok(_) => None,
        };
        if let Some(finding) = finding {
            report.push(finding.in_file(file).at_key(key));
        }
    }

    fn unresolved(
        &self,
        key: &str,
        id: &str,
        client: &str,
        package: Option<&str>,
        error: &anyhow::Error,
    ) -> Finding {
        if list_client_names(self.clients)
            .iter()
            .any(|name| name == client)
        {
            return Finding::of(
                Code::ModelUnresolved,
                format!("{key} '{id}' does not resolve: {error:#}"),
            );
        }
        // A package agent's model loads with its client package-qualified,
        // unless it was written `/client:…` for a top-level client: one the
        // config installing the package provides, which a package checked on
        // its own doesn't have.
        if self.package_mode && package.is_some() && !client.contains('/') {
            return Finding::warning(
                Code::ModelUnresolved,
                format!("{key} '{id}' names top-level client '{client}', which the config that installs this package must provide"),
            );
        }
        Finding::of(
            Code::ModelUnresolved,
            format!("{key} '{id}' names client '{client}', which is not configured"),
        )
    }
}

/// The agent's `model` and `model_fallbacks`, each with its key.
fn agent_models(config: &AgentConfig) -> impl Iterator<Item = (&'static str, &str)> {
    let model = config.model_id().map(|id| ("model", id));
    let fallbacks = config
        .model_fallbacks()
        .iter()
        .map(|id| ("model_fallbacks", id.as_str()));
    model.into_iter().chain(fallbacks)
}

fn not_in_catalog(key: &str, id: &str, client: &str) -> Finding {
    Finding::of(
        Code::ModelNotInCatalog,
        format!(
            "{key} '{id}' is not in the catalog of client '{client}' (models.yaml, models-override.yaml or the client's models:), so harnx sends the name unchecked and without catalog limits"
        ),
    )
}
