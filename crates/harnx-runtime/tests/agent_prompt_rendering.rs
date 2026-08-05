//! Every shipped agent must load and render its prompt through the same code
//! the runtime uses at activation time.
//!
//! Resolving `variables: [{name, path}]` by hand in the test would only prove
//! the markdown files line up; it would not catch a caller that installs an
//! agent without running the resolver. So this test installs the packages into
//! a temp config dir and drives `resolve_variables` — the production path — for
//! each agent.

use std::{ffi::OsStr, fs, path::Path, path::PathBuf};

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
    let Some(workspace_root) = workspace_root() else {
        // Published crates do not include workspace package assets.
        return;
    };
    let (_temp, _config_guard) = install_packages(&workspace_root);

    let mut failures = Vec::new();
    let mut totals = AgentOutcome::default();
    let mut rendered = 0usize;

    for package in PACKAGES {
        let agents_dir = workspace_root.join("packages").join(package).join("agents");
        for stem in agent_stems(&agents_dir, &mut failures) {
            let outcome = check_agent(&format!("{package}/{stem}"), &mut failures);
            totals.file_backed_variables += outcome.file_backed_variables;
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
        totals.file_backed_variables > 0,
        "no shipped agent exercised a file-backed variable"
    );
}
