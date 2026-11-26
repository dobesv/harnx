//! Config the loaders accept without a word, reported as findings: what they
//! skip, drop or replace with a default, so a run that passes `harnx
//! validate` doesn't quietly lose it.
//!
//! Each check calls the function the loader itself calls, so the two can't
//! disagree about what is dropped. Parse errors in the files involved are
//! already reported per file; what is reported here is the consequence.
//!
//! Left out on purpose: a package patch that fails for a tool server stops
//! the whole load, so it already shows as a load error; invalid bash tool
//! templates live in harnx-bash-tools, which this crate doesn't depend on,
//! and their syntax is checked per file.

use std::path::{Path, PathBuf};

use harnx_core::config_paths::{self, CLIENTS_DIR_NAME};
use harnx_core::package::PackagePatch;
use harnx_core::package_namespace::resolve_package_relative_name;
use serde_yaml::Value;

use super::{CheckContext, Code, ConfigLayout, Finding, PackageLayout, Report};
use crate::client::ClientConfig;
use crate::config::agent::list_agents;
use crate::config::session_ops_title::title_agent_lookup_name;
use crate::config::{apply_client_patch, classify_env_line, Config, EnvLine, ModelsOverride};

pub fn check_silent_drops(context: &CheckContext<'_>, report: &mut Report) {
    let layout = context.layout;
    for package in &layout.packages {
        check_package_clients(layout, package, report);
    }
    check_models_override(layout, report);
    check_env_file(layout, report);
    check_front_matter_position(layout, report);
    check_agent_references(layout, report);
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// A package's clients load together: one file that fails drops them all.
/// When they do load, the package patch runs on each, and a client whose
/// patch fails is dropped alone.
fn check_package_clients(layout: &ConfigLayout, package: &PackageLayout, report: &mut Report) {
    let clients_dir = package.dir.join(CLIENTS_DIR_NAME);
    match Config::load_clients_from_dir(&clients_dir) {
        Ok(clients) => check_client_patch(layout, package, clients, report),
        Err(error) => report.push(whole_package_drop(layout, package, &clients_dir, &error)),
    }
}

/// The outermost context of the error names the file; its cause is that
/// file's own parse-error finding.
fn whole_package_drop(
    layout: &ConfigLayout,
    package: &PackageLayout,
    clients_dir: &Path,
    error: &anyhow::Error,
) -> Finding {
    let cause = relative_message(layout, &error.to_string());
    Finding::error(
        Code::SilentDrop,
        format!(
            "all clients in package '{}' are dropped at runtime: {cause}",
            package.name
        ),
    )
    .in_file(layout.relative(clients_dir))
}

fn check_client_patch(
    layout: &ConfigLayout,
    package: &PackageLayout,
    clients: Vec<ClientConfig>,
    report: &mut Report,
) {
    let Some(patch_file) = &package.patch_file else {
        return;
    };
    let Some(patch) =
        read(patch_file).and_then(|text| serde_yaml::from_str::<PackagePatch>(&text).ok())
    else {
        return;
    };
    for mut client in clients {
        let name = resolve_package_relative_name(client.effective_name(), Some(&package.name));
        let Err(error) = apply_client_patch(&mut client, &patch.clients) else {
            continue;
        };
        let message = format!(
            "the patch for package '{}' fails on client '{name}', so the client is dropped at runtime: {error:#}",
            package.name
        );
        report.push(Finding::error(Code::SilentDrop, message).in_file(layout.relative(patch_file)));
    }
}

/// `message` with paths under the checked directory written relative to it.
fn relative_message(layout: &ConfigLayout, message: &str) -> String {
    let root: PathBuf = layout.root.components().collect();
    message.replace(
        &format!("{}{}", root.display(), std::path::MAIN_SEPARATOR),
        "",
    )
}

/// The loader ignores a catalog written for another version.
fn check_models_override(layout: &ConfigLayout, report: &mut Report) {
    let Some(file) = &layout.models_override else {
        return;
    };
    let Some(parsed) =
        read(file).and_then(|text| serde_yaml::from_str::<ModelsOverride>(&text).ok())
    else {
        return;
    };
    let expected = env!("CARGO_PKG_VERSION");
    if parsed.version != expected {
        report.push(
            Finding::warning(
                Code::ModelsOverrideVersion,
                format!(
                    "models-override.yaml is for version {}, not {expected}, so harnx ignores it",
                    parsed.version
                ),
            )
            .in_file(layout.relative(file)),
        );
    }
}

/// Lines the `.env` loader skips. Checked only when the file is one this
/// directory owns or `HARNX_ENV_FILE` names it, so a check of some other
/// directory doesn't judge the user's own `.env`.
fn check_env_file(layout: &ConfigLayout, report: &mut Report) {
    let file = config_paths::env_file();
    if !file.starts_with(&layout.root) && std::env::var_os("HARNX_ENV_FILE").is_none() {
        return;
    }
    let Some(content) = read(&file) else {
        return;
    };
    for (index, line) in content.lines().enumerate() {
        if classify_env_line(line) == EnvLine::Invalid {
            report.push(
                Finding::warning(
                    Code::EnvLineInvalid,
                    "line is not KEY=value, so harnx skips it".to_string(),
                )
                .in_file(layout.relative(&file))
                .at_line(index + 1),
            );
        }
    }
}

/// Every agent file, local and in packages.
fn agent_files(layout: &ConfigLayout) -> impl Iterator<Item = &PathBuf> {
    layout
        .agents
        .iter()
        .chain(layout.packages.iter().flat_map(|package| &package.agents))
}

/// Front matter is read only at byte 0; after a blank line or a byte-order
/// mark the whole file is prompt.
fn check_front_matter_position(layout: &ConfigLayout, report: &mut Report) {
    for file in agent_files(layout) {
        let Some(content) = read(file) else {
            continue;
        };
        if content.starts_with("---") {
            continue;
        }
        let first = content
            .lines()
            .enumerate()
            .map(|(index, line)| (index, line.trim_start_matches('\u{feff}').trim()))
            .find(|(_, line)| !line.is_empty());
        if let Some((index, "---")) = first {
            report.push(
                Finding::warning(
                    Code::FrontMatterNotAtStart,
                    "front matter does not start at the first byte of the file (a blank line or byte-order mark comes first), so harnx treats the whole file as prompt",
                )
                .in_file(layout.relative(file))
                .at_line(index + 1),
            );
        }
    }
}

/// Whether the runtime would find an agent called `name`, local, in a
/// package or remote.
fn agent_exists(name: &str) -> bool {
    Config::agent_file(name).is_file() || list_agents().iter().any(|agent| agent == name)
}

/// `title_agent` and `compaction_agent` that name no agent fall back to the
/// default prompt. A name with `@` points at a remote cluster, which the
/// check can't see.
fn check_agent_references(layout: &ConfigLayout, report: &mut Report) {
    for reference in agent_references(layout) {
        check_reference(layout, &reference, report);
    }
}

/// One `title_agent` or `compaction_agent` value and where it was written.
struct AgentReference {
    file: PathBuf,
    key: &'static str,
    name: String,
    /// The package the file is in, which bare names resolve against.
    package: Option<String>,
}

/// Every such value in `config.yaml` and the agents' front matter.
fn agent_references(layout: &ConfigLayout) -> Vec<AgentReference> {
    let mut references = Vec::new();
    if let Some(file) = &layout.config_file {
        let text = read(file).unwrap_or_default();
        references.extend(references_in(file, &text, None, &["title_agent"]));
    }
    let local = layout.agents.iter().map(|file| (file, None));
    let packaged = layout.packages.iter().flat_map(|p| {
        p.agents
            .iter()
            .map(move |file| (file, Some(p.name.as_str())))
    });
    for (file, package) in local.chain(packaged) {
        let content = read(file).unwrap_or_default();
        let front_matter = harnx_core::agent_config::agent_front_matter(&content);
        let text = front_matter.map(|(text, _)| text).unwrap_or_default();
        references.extend(references_in(
            file,
            text,
            package,
            &["compaction_agent", "title_agent"],
        ));
    }
    references
}

fn references_in(
    file: &Path,
    yaml: &str,
    package: Option<&str>,
    keys: &[&'static str],
) -> Vec<AgentReference> {
    let Ok(value) = serde_yaml::from_str::<Value>(yaml) else {
        return Vec::new();
    };
    keys.iter()
        .filter_map(|&key| {
            let name = value.get(key)?.as_str()?;
            Some(AgentReference {
                file: file.to_path_buf(),
                key,
                name: name.to_string(),
                package: package.map(str::to_string),
            })
        })
        .collect()
}

fn check_reference(layout: &ConfigLayout, reference: &AgentReference, report: &mut Report) {
    let AgentReference {
        file,
        key,
        name,
        package,
    } = reference;
    if name.contains('@') {
        return;
    }
    let resolved = resolve_package_relative_name(name, package.as_deref());
    // Titles look for the package-relative name first, then the top level;
    // compaction looks only at the package-relative one.
    let found = if *key == "title_agent" {
        let lookup = title_agent_lookup_name(name, &resolved, agent_exists(&resolved));
        agent_exists(lookup)
    } else {
        agent_exists(&resolved)
    };
    if !found {
        report.push(
            Finding::error(
                Code::AgentNotFound,
                format!(
                    "{key} '{name}' is not an agent, so harnx falls back to its default prompt"
                ),
            )
            .in_file(layout.relative(file))
            .at_key(*key),
        );
    }
}
