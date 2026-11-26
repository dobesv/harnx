//! References from the checked files to things defined elsewhere: agents
//! named by access rules, environment variables in `nats_servers/`, and the
//! commands tool servers run.

use std::path::{Path, PathBuf};

use harnx_core::access_rules::AccessRules;

use super::{CheckContext, Code, ConfigLayout, Finding, Report};
use crate::config::agent::list_agents;
use crate::config::{Config, NatsServerConfig, ToolServerConfig};
use crate::nats_worker::tool_supervisor::resolve_tool_binary;

/// `config` is the loaded directory, `None` when it doesn't load; tool
/// servers are checked only when it does, since only the loader applies
/// package patches to them.
pub fn check_references(context: &CheckContext<'_>, config: Option<&Config>, report: &mut Report) {
    let layout = context.layout;
    check_access_agents(layout, report);
    for file in &layout.nats_servers {
        check_nats_env(layout, file, report);
    }
    for server in config.into_iter().flat_map(|config| &config.tool_servers) {
        check_command(layout, server, report);
    }
}

/// Rules may be written ahead of their agent, so a pattern that matches no
/// agent, local, package or remote, is a warning. A file that doesn't load
/// already has its parse error.
fn check_access_agents(layout: &ConfigLayout, report: &mut Report) {
    let Some(file) = &layout.access else {
        return;
    };
    let Some(rules) = std::fs::read_to_string(file)
        .ok()
        .and_then(|yaml| AccessRules::from_yaml(&yaml).ok())
    else {
        return;
    };
    let agents = list_agents();
    for (index, pattern) in rules.agent_patterns_matching_none(&agents) {
        report.push(
            Finding::of(
                Code::AccessAgentUnmatched,
                format!("access rule {index}: agents pattern '{pattern}' matches no local, package or remote agent"),
            )
            .in_file(layout.relative(file))
            .at_key(format!("rules.{index}.agents")),
        );
    }
}

/// A variable that isn't set leaves the whole value unexpanded when the
/// file loads. CI often lacks the secrets, so this is a warning.
fn check_nats_env(layout: &ConfigLayout, file: &Path, report: &mut Report) {
    let Some(server) = std::fs::read_to_string(file)
        .ok()
        .and_then(|yaml| serde_yaml::from_str::<NatsServerConfig>(&yaml).ok())
    else {
        return;
    };
    for (key, unset) in Config::unset_nats_server_envs(&server) {
        for variable in unset {
            report.push(
                Finding::of(
                    Code::EnvVarUnset,
                    format!("{key} uses ${{{variable}}}, which is not set in this environment, so harnx leaves {key} unexpanded"),
                )
                .in_file(layout.relative(file))
                .at_key(key),
            );
        }
    }
}

/// Whether an enabled tool server's command can't be found where the worker
/// looks for it, so the server can't start and its tools can't be listed.
pub(crate) fn command_missing(server: &ToolServerConfig) -> bool {
    server.enabled && resolve_tool_binary(server).is_err()
}

fn check_command(layout: &ConfigLayout, server: &ToolServerConfig, report: &mut Report) {
    if !command_missing(server) {
        return;
    }
    let finding = Finding::of(
        Code::CommandNotFound,
        format!(
            "command '{}' of tool server '{}' is not found next to harnx or on PATH, so the server can't start and its tools can't be checked",
            server.command, server.name
        ),
    );
    report.push(match tool_server_file(layout, server) {
        Some(file) => finding.in_file(layout.relative(&file)),
        None => finding,
    });
}

/// The file a loaded tool server came from: the loader names each server
/// after its file's stem.
pub(super) fn tool_server_file(
    layout: &ConfigLayout,
    server: &ToolServerConfig,
) -> Option<PathBuf> {
    let files = match &server.package {
        Some(name) => {
            &layout
                .packages
                .iter()
                .find(|package| &package.name == name)?
                .tool_servers
        }
        None => &layout.tool_servers,
    };
    files
        .iter()
        .find(|file| file.file_stem().and_then(|stem| stem.to_str()) == Some(&server.name))
        .cloned()
}
