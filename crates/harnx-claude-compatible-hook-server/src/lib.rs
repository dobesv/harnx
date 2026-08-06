//! NATS hook server for Claude Code-compatible command hooks.

use anyhow::{bail, Result};
use async_trait::async_trait;
use clap::{Parser, ValueEnum};
use harnx_core::hooks::{HookOutcome, HookPayload};
use harnx_hooks::{execute_command_hook, HookCommand, PersistentHookManager};
use harnx_hookset::{FailPolicy, Hook, HookSpec, HARNX_HOOK_NAME};
use std::path::PathBuf;
use tokio::sync::Mutex;

/// Command-line settings for one hook server.
#[derive(Clone, Debug, Parser)]
#[command(about = "Serve a Claude Code-compatible command hook over NATS")]
pub struct Args {
    /// Stable hook server name used in NATS subjects and registration.
    #[arg(long, env = HARNX_HOOK_NAME)]
    pub name: String,

    /// Hook event to register, such as PreToolUse or PostToolUse.
    #[arg(long)]
    pub event: String,

    /// Optional regular expression matched against the bare tool name.
    #[arg(long)]
    pub matcher: Option<String>,

    /// Dispatch priority. Lower values run first.
    #[arg(long, default_value_t = 0)]
    pub priority: i32,

    /// Hook execution timeout in seconds.
    #[arg(long)]
    pub timeout: Option<u64>,

    /// Failure policy advertised to dispatchers.
    #[arg(long, value_enum, default_value_t = CliFailPolicy::Closed)]
    pub fail_policy: CliFailPolicy,

    /// Keep one hook subprocess alive across requests.
    #[arg(long)]
    pub persistent: bool,

    /// Package directory exposed to the command as HARNX_PACKAGE_DIR.
    #[arg(long)]
    pub package_dir: Option<PathBuf>,

    /// Command run for hook requests, given after `--` as a program and its
    /// arguments. Executed directly, so a hook wanting pipes, redirection or
    /// variable expansion asks for a shell explicitly: `-- sh -c '...'`.
    #[arg(trailing_var_arg = true, required = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

/// Hook dispatch behavior when the server fails.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum CliFailPolicy {
    Closed,
    Open,
}

impl From<CliFailPolicy> for FailPolicy {
    fn from(value: CliFailPolicy) -> Self {
        match value {
            CliFailPolicy::Closed => Self::Closed,
            CliFailPolicy::Open => Self::Open,
        }
    }
}

/// One configured command hook hosted over NATS.
pub struct ClaudeCompatibleHook {
    name: String,
    spec: HookSpec,
    persistent: bool,
    command: HookCommand,
    manager: Mutex<PersistentHookManager>,
}

impl TryFrom<Args> for ClaudeCompatibleHook {
    type Error = anyhow::Error;

    fn try_from(args: Args) -> Result<Self> {
        if args.name.trim().is_empty() {
            bail!("hook server name must not be empty");
        }
        if args.event.trim().is_empty() {
            bail!("hook event must not be empty");
        }
        if args.command.iter().all(|word| word.trim().is_empty()) {
            bail!("hook command must not be empty");
        }

        Ok(Self {
            name: args.name,
            spec: HookSpec {
                event: args.event,
                matcher: args.matcher,
                priority: args.priority,
                timeout_secs: args.timeout,
                fail_policy: args.fail_policy.into(),
            },
            persistent: args.persistent,
            command: HookCommand {
                argv: args.command,
                timeout: args.timeout,
                package_dir: args.package_dir,
            },
            manager: Mutex::new(PersistentHookManager::new()),
        })
    }
}

#[async_trait]
impl Hook for ClaudeCompatibleHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn hooks(&self) -> Vec<HookSpec> {
        vec![self.spec.clone()]
    }

    async fn handle_hook(&self, payload: HookPayload) -> HookOutcome {
        if self.persistent {
            self.manager
                .lock()
                .await
                .send_event(&payload, &self.command)
                .await
        } else {
            execute_command_hook(&payload, &self.command).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use harnx_core::hooks::{HookEvent, HookResultControl};
    #[cfg(unix)]
    use serde_json::json;
    #[cfg(unix)]
    use std::path::Path;

    #[cfg(unix)]
    fn payload(cwd: &Path, value: u64) -> HookPayload {
        HookPayload {
            session_id: format!("session-{value}"),
            cwd: cwd.to_path_buf(),
            resume_count: 0,
            hook_event: HookEvent::PreToolUse {
                tool_name: "exec".to_string(),
                tool_input: json!({"value": value}),
                tool_use_id: format!("tool-use-{value}"),
            },
        }
    }

    fn hook(command: &[&str], persistent: bool) -> ClaudeCompatibleHook {
        Args {
            name: "test-hook".to_string(),
            event: "PreToolUse".to_string(),
            matcher: Some("exec".to_string()),
            priority: 7,
            timeout: Some(5),
            fail_policy: CliFailPolicy::Closed,
            persistent,
            command: command.iter().map(|word| word.to_string()).collect(),
            package_dir: None,
        }
        .try_into()
        .expect("valid test hook")
    }

    /// Hooks needing shell syntax now ask for a shell explicitly.
    #[cfg(unix)]
    fn shell_hook(script: &str, persistent: bool) -> ClaudeCompatibleHook {
        hook(&["sh", "-c", script], persistent)
    }

    #[test]
    fn cli_uses_persistent_flag_instead_of_type() {
        let args = Args::try_parse_from([
            "hook-server",
            "--name",
            "test-hook",
            "--event",
            "PreToolUse",
            "--persistent",
            "--",
            "true",
        ])
        .expect("parse persistent hook");
        assert!(args.persistent);
        assert_eq!(args.command, vec!["true".to_string()]);

        assert!(Args::try_parse_from([
            "hook-server",
            "--name",
            "test-hook",
            "--event",
            "PreToolUse",
            "--type",
            "persistent",
            "--",
            "true",
        ])
        .is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn one_shot_exit_zero_parses_hook_result() {
        let cwd = tempfile::tempdir().expect("temp dir");
        let runner = hook(
            &["printf", "%s", r#"{"additionalContext":"from-hook"}"#],
            false,
        );

        let outcome = runner.handle_hook(payload(cwd.path(), 1)).await;

        assert_eq!(outcome.control, HookResultControl::Continue);
        assert_eq!(
            outcome.result.additional_context.as_deref(),
            Some("from-hook")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn one_shot_exit_two_blocks_with_stderr() {
        let cwd = tempfile::tempdir().expect("temp dir");
        let runner = shell_hook("printf '%s' 'denied by test' >&2; exit 2", false);

        let outcome = runner.handle_hook(payload(cwd.path(), 1)).await;

        assert_eq!(
            outcome.control,
            HookResultControl::Block {
                reason: "denied by test".to_string()
            }
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn one_shot_returns_mutated_tool_input() {
        let cwd = tempfile::tempdir().expect("temp dir");
        let runner = hook(
            &["printf", "%s", r#"{"mutatedToolInput":{"command":"safe"}}"#],
            false,
        );

        let outcome = runner.handle_hook(payload(cwd.path(), 1)).await;

        assert_eq!(
            outcome.result.mutated_tool_input,
            Some(json!({"command": "safe"}))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persistent_process_routes_two_id_framed_round_trips() {
        let cwd = tempfile::tempdir().expect("temp dir");
        let runner = shell_hook(
            r#"count=0; while IFS= read -r line; do count=$((count + 1)); id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p'); printf '{"id":"%s","mutatedToolInput":{"sequence":%s}}\n' "$id" "$count"; done"#,
            true,
        );

        let first = runner.handle_hook(payload(cwd.path(), 1)).await;
        let second = runner.handle_hook(payload(cwd.path(), 2)).await;

        assert_eq!(
            first.result.mutated_tool_input,
            Some(json!({"sequence": 1}))
        );
        assert_eq!(
            second.result.mutated_tool_input,
            Some(json!({"sequence": 2}))
        );
    }

    #[test]
    fn hook_metadata_comes_from_configuration() {
        let runner = hook(&["true"], false);
        assert_eq!(runner.name(), "test-hook");
        assert_eq!(
            runner.hooks(),
            vec![HookSpec {
                event: "PreToolUse".to_string(),
                matcher: Some("exec".to_string()),
                priority: 7,
                timeout_secs: Some(5),
                fail_policy: FailPolicy::Closed,
            }]
        );
    }
}
