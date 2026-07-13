//! Hook execution for harnx-sandbox-run.
//!
//! Converts CLI hook definitions into `HookConfig` and dispatches them via
//! the harnx-hooks crate. Extracts environment mutations from hook output.
//!
//! The `PersistentHookManager` is returned alongside the env map so the caller
//! can keep sidecar processes (e.g. harnx-proxy-auth) alive for the duration
//! of the sandboxed command. The caller must drop the manager (and the tokio
//! runtime) only after the child process has exited.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Result};
use harnx_core::hooks::{HookConfig, HookEvent, HookOutcome, HookResultControl};
use harnx_hooks::{dispatch_hooks_with_managers, PersistentHookManager};
use serde_json::json;
use tokio::sync::Mutex as TokioMutex;

use crate::cli::HookDef;

/// Result of hook dispatch: env mutations plus the live persistent manager.
///
/// The manager must be kept alive (not dropped) for the entire duration of the
/// sandboxed child process — sidecar hooks like `harnx-proxy-auth` run inside
/// it. Drop the manager (and the tokio runtime) only after the child exits.
pub struct HookResult {
    /// Environment variable mutations to apply to the sandbox.
    pub env: HashMap<String, String>,
    /// Live persistent hook manager — drop this after the child exits.
    pub manager: Arc<TokioMutex<PersistentHookManager>>,
}

/// Start persistent hooks, dispatch one PreToolUse event to collect env
/// mutations, and return both the env map and the live manager.
///
/// The manager is NOT shut down here — the caller keeps it alive.
pub async fn run_hooks(
    hook_defs: &[HookDef],
    session_id: &str,
    cwd: &Path,
    command: &[OsString],
) -> Result<HookResult> {
    if hook_defs.is_empty() {
        return Ok(HookResult {
            env: HashMap::new(),
            manager: Arc::new(TokioMutex::new(PersistentHookManager::new())),
        });
    }

    // Convert HookDefs to HookConfigs
    let hooks: Vec<HookConfig> = hook_defs
        .iter()
        .map(|def| {
            let full_command = build_hook_command(&def.command, &def.args);
            HookConfig {
                event: "PreToolUse".to_string(),
                matcher: None,
                command: full_command,
                timeout: Some(30),
                status_message: None,
                async_hook: Some(false),
                hook_type: def.hook_type.clone(),
                package_dir: None,
            }
        })
        .collect();

    let command_str: String = command
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");

    let tool_use_id = uuid::Uuid::new_v4().to_string();

    let event = HookEvent::PreToolUse {
        tool_name: "exec".to_string(),
        tool_use_id,
        tool_input: json!({
            "command": command_str,
            "env": {},
        }),
    };

    let persistent_manager = Arc::new(TokioMutex::new(PersistentHookManager::new()));

    let outcome = dispatch_hooks_with_managers(
        &event,
        &hooks,
        session_id,
        cwd,
        None,
        Some(&persistent_manager),
    )
    .await;

    // Check for block — shut down before bailing
    if matches!(outcome.control, HookResultControl::Block { .. }) {
        persistent_manager.lock().await.shutdown();
        bail!(
            "hook blocked execution: {}",
            outcome
                .result
                .additional_context
                .as_deref()
                .unwrap_or("no reason")
        );
    }

    let env = extract_hook_env(&outcome);

    // Return the manager alive — caller keeps it until the child exits.
    Ok(HookResult {
        env,
        manager: persistent_manager,
    })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn build_hook_command(command: &str, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(shell_quote(command));
    for arg in args {
        parts.push(shell_quote(arg));
    }
    parts.join(" ")
}

fn extract_env_from_value(env: &mut HashMap<String, String>, value: &serde_json::Value) {
    if let Some(env_map) = value.get("env").and_then(|env_obj| env_obj.as_object()) {
        for (key, value) in env_map {
            if let Some(value) = value.as_str() {
                env.insert(key.clone(), value.to_string());
            }
        }
    }
}

pub(crate) fn extract_hook_env(outcome: &HookOutcome) -> HashMap<String, String> {
    let mut env = HashMap::new();

    if let Some(hso) = &outcome.result.hook_specific_output {
        if let Some(tool_input) = &hso.tool_input {
            extract_env_from_value(&mut env, tool_input);
        }
    }

    if let Some(tool_input) = &outcome.result.mutated_tool_input {
        extract_env_from_value(&mut env, tool_input);
    }

    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::hooks::{HookOutcome, HookResult, HookSpecificOutput};
    use serde_json::json;

    #[test]
    fn shell_quotes_hook_command_and_args() {
        assert_eq!(
            build_hook_command(
                "/tmp/hook script",
                &["arg with spaces".to_string(), "it's fine".to_string()]
            ),
            "'/tmp/hook script' 'arg with spaces' 'it'\\''s fine'"
        );
    }

    #[test]
    fn extracts_env_from_hook_specific_output() {
        let outcome = HookOutcome {
            control: HookResultControl::Continue,
            result: HookResult {
                hook_specific_output: Some(HookSpecificOutput {
                    tool_input: Some(json!({
                        "command": "env",
                        "env": {
                            "HOOK_TEST_VAR": "from_hook"
                        }
                    })),
                    ..HookSpecificOutput::default()
                }),
                ..HookResult::default()
            },
        };

        let env = extract_hook_env(&outcome);
        assert_eq!(
            env.get("HOOK_TEST_VAR").map(String::as_str),
            Some("from_hook")
        );
    }

    #[test]
    fn mutated_tool_input_overrides_hook_specific_output_env() {
        let outcome = HookOutcome {
            control: HookResultControl::Continue,
            result: HookResult {
                hook_specific_output: Some(HookSpecificOutput {
                    tool_input: Some(json!({"env": {"HOOK_TEST_VAR": "old"}})),
                    ..HookSpecificOutput::default()
                }),
                mutated_tool_input: Some(
                    json!({"env": {"HOOK_TEST_VAR": "new", "OTHER": "value"}}),
                ),
                ..HookResult::default()
            },
        };

        let env = extract_hook_env(&outcome);
        assert_eq!(env.get("HOOK_TEST_VAR").map(String::as_str), Some("new"));
        assert_eq!(env.get("OTHER").map(String::as_str), Some("value"));
    }
}
