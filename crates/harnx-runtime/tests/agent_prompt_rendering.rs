//! Every shipped agent must load and render its prompt through the same code
//! the runtime uses at activation time.
//!
//! Resolving `variables: [{name, path}]` by hand in the test would only prove
//! the markdown files line up; it would not catch a caller that installs an
//! agent without running the resolver. So this test installs the packages into
//! a temp config dir and drives `resolve_variables` — the production path — for
//! each agent.

use std::{ffi::OsStr, fs, path::Path, path::PathBuf};

use harnx_runtime::client::{retrieve_model, ModelType};
use harnx_runtime::config::agent::{load_with_qualified_name, resolve_variables};
use harnx_runtime::config::Config;

const PACKAGES: [&str; 2] = ["pantheon", "coding"];

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set_path(key: &'static str, value: &Path) -> Self {
        let previous = std::env::var_os(key);
        // SAFETY: nextest gives each test its own process, so nothing else
        // mutates the environment concurrently.
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: see EnvVarGuard::set_path.
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

fn workspace_root() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|ancestor| ancestor.join("packages").is_dir())
        .map(Path::to_path_buf)
}

/// Point `HARNX_CONFIG_DIR` at a temp dir whose `packages/` mirrors the
/// workspace's, so `Config::agent_file("pantheon/sisyphus")` resolves to the
/// real shipped agent.
fn install_packages(workspace_root: &Path) -> (tempfile::TempDir, EnvVarGuard) {
    let temp = tempfile::tempdir().expect("temp config dir");
    let packages_dir = temp.path().join("packages");
    fs::create_dir_all(&packages_dir).expect("create packages dir");
    for package in PACKAGES {
        let source = workspace_root.join("packages").join(package);
        let link = packages_dir.join(package);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, &link).expect("link package into config dir");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&source, &link).expect("link package into config dir");
    }
    let guard = EnvVarGuard::set_path("HARNX_CONFIG_DIR", temp.path());
    (temp, guard)
}

fn agent_stems(agents_dir: &Path, failures: &mut Vec<String>) -> Vec<String> {
    let entries = match fs::read_dir(agents_dir) {
        Ok(entries) => entries,
        Err(error) => {
            failures.push(format!("{}: {error}", agents_dir.display()));
            return Vec::new();
        }
    };
    let mut stems: Vec<String> = entries
        .filter_map(|entry| match entry {
            Ok(entry) => {
                let path = entry.path();
                if path.is_file() && path.extension() == Some(OsStr::new("md")) {
                    path.file_stem()
                        .and_then(OsStr::to_str)
                        .map(ToOwned::to_owned)
                } else {
                    None
                }
            }
            Err(error) => {
                failures.push(format!("{}: {error}", agents_dir.display()));
                None
            }
        })
        .collect();
    stems.sort();
    stems
}

/// What one agent contributed to the run: how many `path:` variables it
/// resolved, and whether its prompt rendered.
#[derive(Default)]
struct AgentOutcome {
    file_backed_variables: usize,
    rendered: bool,
}

/// Every `path:` variable must come back with content. An empty value means
/// the resolver ran but did not actually read the file.
fn check_file_backed_variables(
    agent: &harnx_runtime::config::Agent,
    qualified_name: &str,
    failures: &mut Vec<String>,
) -> usize {
    let declared: Vec<&str> = agent
        .defined_variables()
        .iter()
        .filter(|variable| variable.path.is_some())
        .map(|variable| variable.name.as_str())
        .collect();
    for name in &declared {
        let resolved = agent.variables().get(*name).is_some_and(|v| !v.is_empty());
        if !resolved {
            failures.push(format!(
                "{qualified_name}: variable '{name}' declares a path but resolved to nothing"
            ));
        }
    }
    declared.len()
}

fn check_agent(qualified_name: &str, failures: &mut Vec<String>) -> AgentOutcome {
    let agent_path = Config::agent_file(qualified_name);
    let mut agent = match load_with_qualified_name(&agent_path, qualified_name) {
        Ok(agent) => agent,
        Err(error) => {
            failures.push(format!("{qualified_name}: {error:#}"));
            return AgentOutcome::default();
        }
    };
    if let Err(error) = resolve_variables(&mut agent) {
        failures.push(format!("{qualified_name}: {error:#}"));
        return AgentOutcome::default();
    }

    let file_backed_variables = check_file_backed_variables(&agent, qualified_name, failures);
    let rendered = match agent.system_text() {
        Ok(_) => true,
        Err(error) => {
            failures.push(format!("{qualified_name}: {error:#}"));
            false
        }
    };
    AgentOutcome {
        file_backed_variables,
        rendered,
    }
}

#[test]
fn agent_prompt_rendering_renders_all_shipped_agents() {
    // `install_packages` sets HARNX_CONFIG_DIR process-wide; the guards are
    // only sound because nextest runs each test in its own process.
    harnx_core::require_nextest();
    let Some(workspace_root) = workspace_root() else {
        // Published crates do not include workspace package assets.
        return;
    };
    let (_temp, _config_guard) = install_packages(&workspace_root);

    let mut failures = Vec::new();
    let mut file_backed_variables = 0usize;
    let mut rendered = 0usize;

    for package in PACKAGES {
        let agents_dir = workspace_root.join("packages").join(package).join("agents");
        for stem in agent_stems(&agents_dir, &mut failures) {
            let outcome = check_agent(&format!("{package}/{stem}"), &mut failures);
            file_backed_variables += outcome.file_backed_variables;
            rendered += usize::from(outcome.rendered);
        }
    }

    assert!(
        failures.is_empty(),
        "failed to load or render shipped agent prompts:\n{}",
        failures.join("\n")
    );
    assert!(rendered > 0, "no shipped agents were rendered");
    assert!(
        file_backed_variables > 0,
        "no shipped agent exercised a file-backed variable"
    );
}

fn check_agent_models(config: &Config, qualified_name: &str) -> (usize, Vec<String>) {
    let agent_path = Config::agent_file(qualified_name);
    let agent = match load_with_qualified_name(&agent_path, qualified_name) {
        Ok(agent) => agent,
        Err(error) => return (0, vec![format!("{qualified_name}: {error:#}")]),
    };
    let model_ids = agent
        .model_id()
        .into_iter()
        .chain(agent.model_fallbacks().iter().map(String::as_str));
    let mut checked = 0;
    let mut failures = Vec::new();
    for model_id in model_ids {
        checked += 1;
        if let Err(error) = check_model_metadata(config, model_id) {
            failures.push(format!("{qualified_name}: {error}"));
        }
    }
    (checked, failures)
}

fn check_model_metadata(config: &Config, model_id: &str) -> Result<(), String> {
    let model = retrieve_model(&config.clients, model_id, ModelType::Chat)
        .map_err(|error| format!("model {model_id} does not resolve: {error:#}"))?;
    if model.real_name() != "gpt-5.6-sol" {
        return Ok(());
    }
    if model.endpoint() != Some("responses") {
        return Err(format!("{model_id} lost its Responses endpoint metadata"));
    }
    Ok(())
}

#[test]
fn shipped_agent_models_resolve_with_required_endpoint_metadata() {
    harnx_core::require_nextest();
    let Some(workspace_root) = workspace_root() else {
        return;
    };
    let (temp, _config_guard) = install_packages(&workspace_root);
    let config = Config::load_from_file(&temp.path().join("config.yaml"))
        .expect("load package client configs");
    let mut failures = Vec::new();
    let mut checked = 0usize;

    for package in PACKAGES {
        let agents_dir = workspace_root.join("packages").join(package).join("agents");
        for stem in agent_stems(&agents_dir, &mut failures) {
            let qualified_name = format!("{package}/{stem}");
            let (agent_checked, agent_failures) = check_agent_models(&config, &qualified_name);
            checked += agent_checked;
            failures.extend(agent_failures);
        }
    }

    assert!(
        checked > 0,
        "no shipped agent model references were checked"
    );
    assert!(
        failures.is_empty(),
        "invalid shipped agent model references:\n{}",
        failures.join("\n")
    );
}

#[test]
fn clio_prompt_checks_for_an_existing_pull_request_after_push() {
    harnx_core::require_nextest();
    let Some(workspace_root) = workspace_root() else {
        return;
    };
    let (_temp, _config_guard) = install_packages(&workspace_root);

    let qualified_name = "pantheon/clio";
    let agent_path = Config::agent_file(qualified_name);
    let mut agent = load_with_qualified_name(&agent_path, qualified_name).expect("load Clio");
    resolve_variables(&mut agent).expect("resolve Clio variables");
    let prompt = agent.system_text().expect("render Clio prompt");

    assert!(
        prompt.contains("gh pr list --head \"$branch\" --state open --limit 1"),
        "Clio must query for an open pull request on the pushed branch"
    );
    assert!(
        prompt.contains("url,state,isDraft,mergeStateStatus,reviewDecision,statusCheckRollup"),
        "Clio must retrieve the existing pull request's link and status"
    );
    assert!(
        prompt.contains("Only when no open pull request exists for the branch"),
        "Clio must reserve the compare-link fallback for branches without an open pull request"
    );
}
