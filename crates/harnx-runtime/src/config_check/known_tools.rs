//! The tool names each package's agents can see: those harnx knows offline
//! and those the servers that listed their tools report.

use std::collections::HashMap;

use harnx_core::package_namespace::handoff_display_name;
use harnx_toolset::{Registration, ToolSpec};

use super::tool_listing::{server_key, ServerListing};
use super::tool_refs::ToolRef;
use crate::config::agent::list_agents;
use crate::config::{
    builtin_tool_names, handoff_tool_name, is_bash_inner_tool_allowed, selector_matcher, Config,
    ToolServerConfig,
};
use crate::nats_worker::subagent_registration;
use crate::server_identity::ServerIdentity;

/// Tool names known in each package's view, keyed by package (`None` for
/// the top level).
pub(super) struct KnownNames {
    by_package: HashMap<Option<String>, Vec<String>>,
}

impl KnownNames {
    /// Names harnx knows without asking any server: built-ins, script tools,
    /// and every agent's sub-agent and handoff tools.
    pub(super) fn offline(config: &Config, refs: &[ToolRef]) -> Self {
        let agents = list_agents();
        let subagents: Vec<Registration> = agents
            .iter()
            .map(|agent| subagent_registration(agent))
            .collect();
        let mut by_package = HashMap::new();
        for package in refs.iter().map(|r| r.agent_package.clone()) {
            by_package.entry(package).or_insert_with_key(|package| {
                let package = package.as_deref();
                let mut names: Vec<String> = builtin_tool_names().map(str::to_string).collect();
                names.extend(
                    config
                        .script_tool_catalog()
                        .iter()
                        .map(|tool| tool.definition().name.clone()),
                );
                names.extend(subagents.iter().flat_map(|r| visible_names(package, r)));
                names.extend(
                    agents
                        .iter()
                        .map(|agent| handoff_tool_name(&handoff_display_name(agent, package))),
                );
                names
            });
        }
        Self { by_package }
    }

    /// Add the tools of every server that listed them, as each package sees
    /// them.
    pub(super) fn with_listings(
        mut self,
        config: &Config,
        listings: &HashMap<String, ServerListing>,
    ) -> Self {
        let registrations: Vec<Registration> = config
            .tool_servers
            .iter()
            .filter_map(|server| listed_registration(server, listings))
            .collect();
        for (package, names) in &mut self.by_package {
            names.extend(
                registrations
                    .iter()
                    .flat_map(|registration| visible_names(package.as_deref(), registration)),
            );
        }
        self
    }

    /// Whether `tool_ref` selects a known name the way the runtime selects
    /// it: a toolset member by exact name, anything else as a glob, and for
    /// selectors scripts resolve, only names a script may call.
    pub(super) fn matches(&self, tool_ref: &ToolRef) -> bool {
        let glob = selector_matcher(&tool_ref.selector);
        let selects = |name: &str| match tool_ref.exact {
            true => name == tool_ref.selector,
            false => glob(name),
        };
        let callable =
            |name: &str| !tool_ref.script_callable_only() || is_bash_inner_tool_allowed(name);
        self.by_package
            .get(&tool_ref.agent_package)
            .is_some_and(|names| names.iter().any(|name| selects(name) && callable(name)))
    }
}

/// The registration a listed server would publish, as the worker builds it.
pub(super) fn listed_registration(
    server: &ToolServerConfig,
    listings: &HashMap<String, ServerListing>,
) -> Option<Registration> {
    let ServerListing::Listed(listing) = listings.get(&server_key(server))? else {
        return None;
    };
    Some(registration(server, &listing.server, listing.tools.clone()))
}

/// The registration `server` publishes when it reports itself as `name`.
pub(super) fn registration(
    server: &ToolServerConfig,
    name: &str,
    tools: Vec<ToolSpec>,
) -> Registration {
    Registration {
        package: server.package.clone(),
        config: server.name.clone(),
        server: name.to_string(),
        tools,
        schema_version: 1,
        proto_version: 1,
        user_id: None,
        ownerless_only: false,
        connection_id: None,
    }
}

pub(super) fn visible_names(package: Option<&str>, registration: &Registration) -> Vec<String> {
    registration
        .tools
        .iter()
        .map(|tool| ServerIdentity::agent_visible_name(package, registration, &tool.name))
        .collect()
}
