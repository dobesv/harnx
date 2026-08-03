//! Hook execution for harnx-sandbox-run.
//!
//! Converts CLI hook definitions into inline hook specs and dispatches them via
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
use harnx_core::hooks::{HookEvent, HookOutcome, HookResultControl};
use harnx_hooks::{
    dispatch_hooks_with_options, DispatchOptions, HookCommand, InlineHookSpec,
    PersistentHookManager,
};
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
        return Ok(empty_hook_result());
    }

    let (hooks, persistent_modes) = build_inline_hooks(hook_defs);
    let event = pre_tool_event(command);
    let persistent_manager = Arc::new(TokioMutex::new(PersistentHookManager::new()));
    let outcome = dispatch_hooks_with_options(
        &event,
        &hooks,
        session_id,
        cwd,
        DispatchOptions {
            persistent_modes: &persistent_modes,
            persistent_manager: Some(&persistent_manager),
            ..DispatchOptions::default()
        },
    )
    .await;

    reject_blocked_hook(&outcome, &persistent_manager).await?;
    Ok(HookResult {
        env: extract_hook_env(&outcome),
        manager: persistent_manager,
    })
}

fn empty_hook_result() -> HookResult {
    HookResult {
        env: HashMap::new(),
        manager: Arc::new(TokioMutex::new(PersistentHookManager::new())),
    }
}

fn build_inline_hooks(hook_defs: &[HookDef]) -> (Vec<InlineHookSpec>, Vec<bool>) {
    hook_defs.iter().filter_map(build_inline_hook).unzip()
}

fn build_inline_hook(def: &HookDef) -> Option<(InlineHookSpec, bool)> {
    let persistent = match def.hook_type.as_str() {
        "claude-command" => false,
        "claude-command-persistent" => true,
        _ => return None,
    };
    let hook = InlineHookSpec {
        event: "PreToolUse".to_string(),
        matcher: None,
        command: HookCommand {
            command: build_hook_command(&def.command, &def.args),
            timeout: Some(30),
            package_dir: None,
        },
        async_hook: Some(false),
    };
    Some((hook, persistent))
}

fn pre_tool_event(command: &[OsString]) -> HookEvent {
    let command = command
        .iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    HookEvent::PreToolUse {
        tool_name: "exec".to_string(),
        tool_use_id: uuid::Uuid::new_v4().to_string(),
        tool_input: json!({
            "command": command,
            "env": {},
        }),
    }
}

async fn reject_blocked_hook(
    outcome: &HookOutcome,
    persistent_manager: &Arc<TokioMutex<PersistentHookManager>>,
) -> Result<()> {
    if !matches!(outcome.control, HookResultControl::Block { .. }) {
        return Ok(());
    }
    persistent_manager.lock().await.shutdown();
    bail!(
        "hook blocked execution: {}",
        outcome
            .result
            .additional_context
            .as_deref()
            .unwrap_or("no reason")
    )
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
