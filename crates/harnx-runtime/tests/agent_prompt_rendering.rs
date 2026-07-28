use std::{ffi::OsStr, fs, path::Path};

use harnx_core::agent_config::{AgentConfig, AgentVariables};

#[test]
fn agent_prompt_rendering_renders_all_shipped_agents() {
    let Some(workspace_root) = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|ancestor| ancestor.join("packages").is_dir())
    else {
        // Published crates do not include workspace package assets.
        return;
    };

    let agent_dirs = [
        workspace_root.join("packages/pantheon/agents"),
        workspace_root.join("packages/coding/agents"),
    ];
    let mut failures = Vec::new();

    for agent_dir in agent_dirs {
        let entries = match fs::read_dir(&agent_dir) {
            Ok(entries) => entries,
            Err(error) => {
                failures.push(format!("{}: {error}", agent_dir.display()));
                continue;
            }
        };
        let mut agent_paths = entries
            .filter_map(|entry| match entry {
                Ok(entry) => {
                    let path = entry.path();
                    (path.is_file() && path.extension() == Some(OsStr::new("md"))).then_some(path)
                }
                Err(error) => {
                    failures.push(format!("{}: {error}", agent_dir.display()));
                    None
                }
            })
            .collect::<Vec<_>>();
        agent_paths.sort();

        for agent_path in agent_paths {
            let content = match fs::read_to_string(&agent_path) {
                Ok(content) => content,
                Err(error) => {
                    failures.push(format!("{}: {error}", agent_path.display()));
                    continue;
                }
            };
            let Some(stem) = agent_path.file_stem().and_then(OsStr::to_str) else {
                failures.push(format!("{}: invalid UTF-8 file stem", agent_path.display()));
                continue;
            };
            let mut agent = match AgentConfig::from_markdown(stem, &content) {
                Ok(agent) => agent,
                Err(error) => {
                    failures.push(format!("{}: {error:#}", agent_path.display()));
                    continue;
                }
            };

            let mut variables = AgentVariables::new();
            let mut variable_loading_failed = false;
            for variable in agent.defined_variables() {
                if let Some(relative_path) = &variable.path {
                    let variable_path = agent_dir.join(relative_path);
                    match fs::read_to_string(&variable_path) {
                        Ok(value) => {
                            variables.insert(variable.name.clone(), value);
                        }
                        Err(error) => {
                            failures.push(format!(
                                "{}: failed to load variable '{}' from {}: {error}",
                                agent_path.display(),
                                variable.name,
                                variable_path.display()
                            ));
                            variable_loading_failed = true;
                        }
                    }
                } else if let Some(default) = &variable.default {
                    variables.insert(variable.name.clone(), default.clone());
                }
            }
            if variable_loading_failed {
                continue;
            }

            agent.set_shared_variables(variables);
            if let Err(error) = agent.system_text() {
                failures.push(format!("{}: {error:#}", agent_path.display()));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "failed to load or render shipped agent prompts:\n{}",
        failures.join("\n")
    );
}
