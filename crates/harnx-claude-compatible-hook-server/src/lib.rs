//! NATS hook server for Claude Code-compatible command hooks.

use anyhow::{bail, Result};
use async_trait::async_trait;
use clap::{Parser, ValueEnum};
use harnx_core::{
    hooks::{HookOutcome, HookPayload, HookResult, HookResultControl},
    jaq::JaqFilter,
};
use harnx_hooks::{
    execute_command_hook, executor::control_from_result, HookCommand, PersistentHookManager,
};
use harnx_hookset::{FailPolicy, Hook, HookSpec, HARNX_HOOK_NAME};
use std::path::PathBuf;
use tokio::sync::Mutex;

/// Command-line settings for one hook server.
#[derive(Clone, Debug, Parser)]
#[command(about = "Serve a Claude Code-compatible command or jaq hook over NATS")]
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
    #[arg(long, conflicts_with = "jaq")]
    pub persistent: bool,

    /// Evaluate an embedded jaq expression for each hook payload. The result
    /// must use the Claude-compatible hook response shape.
    #[arg(long, visible_alias = "jq", conflicts_with = "command")]
    pub jaq: Option<String>,

    /// Package directory exposed to the command as HARNX_PACKAGE_DIR.
    #[arg(long, conflicts_with = "jaq")]
    pub package_dir: Option<PathBuf>,

    /// Command run for hook requests, given after `--` as a program and its
    /// arguments. Executed directly, so a hook wanting pipes, redirection or
    /// variable expansion asks for a shell explicitly: `-- sh -c '...'`.
    #[arg(
        trailing_var_arg = true,
        required_unless_present = "jaq",
        conflicts_with = "jaq",
        value_name = "COMMAND"
    )]
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
    handler: HookHandler,
}

enum HookHandler {
    Command {
        persistent: bool,
        command: HookCommand,
        manager: Mutex<PersistentHookManager>,
    },
    Jaq(JaqFilter),
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
        let handler = match args.jaq {
            Some(expression) => {
                HookHandler::Jaq(JaqFilter::compile(expression).map_err(anyhow::Error::msg)?)
            }
            None => {
                // Only the program word must be present: later arguments may
                // legitimately be empty strings, but an empty argv[0] reaches
                // process creation and fails with a bare ENOENT.
                if args
                    .command
                    .first()
                    .is_none_or(|program| program.trim().is_empty())
                {
                    bail!("hook command must not be empty");
                }
                HookHandler::Command {
                    persistent: args.persistent,
                    command: HookCommand {
                        argv: args.command,
                        timeout: args.timeout,
                        package_dir: args.package_dir,
                    },
                    manager: Mutex::new(PersistentHookManager::new()),
                }
            }
        };

        Ok(Self {
            name: args.name,
            spec: HookSpec {
                event: args.event,
                matcher: args.matcher,
                priority: args.priority,
                timeout_secs: args.timeout,
                fail_policy: args.fail_policy.into(),
            },
            handler,
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
        match &self.handler {
            HookHandler::Command {
                persistent,
                command,
                manager,
            } => {
                if *persistent {
                    manager.lock().await.send_event(&payload, command).await
                } else {
                    execute_command_hook(&payload, command).await
                }
            }
            HookHandler::Jaq(filter) => self.evaluate_jaq(filter, payload),
        }
    }
}

impl ClaudeCompatibleHook {
    fn evaluate_jaq(&self, filter: &JaqFilter, payload: HookPayload) -> HookOutcome {
        let output = serde_json::to_value(payload)
            .map_err(|error| format!("serialize hook payload for jaq: {error}"))
            .and_then(|input| filter.evaluate(input))
            .and_then(|output| {
                serde_json::from_value::<HookResult>(output)
                    .map_err(|error| format!("jaq hook returned an invalid response: {error}"))
            });

        match output {
            Ok(result) => HookOutcome {
                control: control_from_result(&result),
                result,
            },
            Err(error) => {
                log::warn!("{error}");
                match self.spec.fail_policy {
                    FailPolicy::Closed => HookOutcome {
                        control: HookResultControl::Block {
                            reason: format!("jaq hook failed: {error}"),
                        },
                        result: HookResult::default(),
                    },
                    FailPolicy::Open => HookOutcome {
                        control: HookResultControl::Continue,
                        result: HookResult::default(),
                    },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::hooks::{HookEvent, HookResultControl};
    use serde_json::json;
    use std::path::Path;

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
            jaq: None,
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

    fn jaq_hook(expression: &str, fail_policy: CliFailPolicy) -> ClaudeCompatibleHook {
        Args {
            name: "test-jaq-hook".to_string(),
            event: "PreToolUse".to_string(),
            matcher: Some("exec".to_string()),
            priority: 7,
            timeout: Some(5),
            fail_policy,
            persistent: false,
            jaq: Some(expression.to_string()),
            command: Vec::new(),
            package_dir: None,
        }
        .try_into()
        .expect("valid jaq hook")
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
        assert_eq!(args.jaq, None);
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

    #[test]
    fn cli_accepts_jaq_instead_of_a_child_command() {
        let args = Args::try_parse_from([
            "hook-server",
            "--name",
            "test-hook",
            "--event",
            "PreToolUse",
            "--jaq",
            r#"{"additionalContext":"from-jaq"}"#,
        ])
        .expect("parse jaq hook");

        assert_eq!(
            args.jaq.as_deref(),
            Some(r#"{"additionalContext":"from-jaq"}"#)
        );
        assert!(args.command.is_empty());
    }

    #[test]
    fn cli_requires_exactly_one_hook_handler() {
        let base = [
            "hook-server",
            "--name",
            "test-hook",
            "--event",
            "PreToolUse",
        ];
        assert!(Args::try_parse_from(base).is_err());
        assert!(Args::try_parse_from([
            "hook-server",
            "--name",
            "test-hook",
            "--event",
            "PreToolUse",
            "--jaq",
            ".",
            "--",
            "true",
        ])
        .is_err());
    }

    #[test]
    fn invalid_jaq_expression_is_rejected_at_startup() {
        let args = Args {
            name: "test-jaq-hook".to_string(),
            event: "PreToolUse".to_string(),
            matcher: None,
            priority: 0,
            timeout: None,
            fail_policy: CliFailPolicy::Closed,
            persistent: false,
            jaq: Some(".tool_name ==".to_string()),
            command: Vec::new(),
            package_dir: None,
        };

        let error = ClaudeCompatibleHook::try_from(args)
            .err()
            .expect("invalid jaq must fail");
        assert!(error.to_string().contains("jaq parse/compile failed"));
    }

    #[tokio::test]
    async fn jaq_hook_can_request_confirmation_from_payload_fields() {
        let cwd = tempfile::tempdir().expect("temp dir");
        let runner = jaq_hook(
            r#"if .tool_name == "exec" and .tool_input.value == 1 then {"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"test approval"}} else {} end"#,
            CliFailPolicy::Closed,
        );

        let outcome = runner.handle_hook(payload(cwd.path(), 1)).await;

        assert_eq!(
            outcome.control,
            HookResultControl::Ask {
                reason: Some("test approval".to_string())
            }
        );
    }

    #[tokio::test]
    async fn jaq_hook_can_condition_on_a_command_regex() {
        let cwd = tempfile::tempdir().expect("temp dir");
        let runner = jaq_hook(
            r#"if ((.tool_input.command // "") | test("\\b(rm|mv|cp|chmod|chown|dd|mkfs)\\b")) then {"hookSpecificOutput":{"permissionDecision":"ask"}} else {} end"#,
            CliFailPolicy::Closed,
        );
        let mut dangerous = payload(cwd.path(), 1);
        let mut safe = payload(cwd.path(), 2);
        if let HookEvent::PreToolUse { tool_input, .. } = &mut dangerous.hook_event {
            *tool_input = json!({"command": "rm -rf build"});
        }
        if let HookEvent::PreToolUse { tool_input, .. } = &mut safe.hook_event {
            *tool_input = json!({"command": "cargo check"});
        }

        let dangerous_outcome = runner.handle_hook(dangerous).await;
        let safe_outcome = runner.handle_hook(safe).await;

        assert_eq!(
            dangerous_outcome.control,
            HookResultControl::Ask { reason: None }
        );
        assert_eq!(safe_outcome.control, HookResultControl::Continue);
    }

    #[tokio::test]
    async fn jaq_runtime_failure_honors_fail_policy() {
        let cwd = tempfile::tempdir().expect("temp dir");
        let closed = jaq_hook(".tool_input.value[]", CliFailPolicy::Closed);
        let open = jaq_hook(".tool_input.value[]", CliFailPolicy::Open);

        let closed_outcome = closed.handle_hook(payload(cwd.path(), 1)).await;
        let open_outcome = open.handle_hook(payload(cwd.path(), 1)).await;

        assert!(matches!(
            closed_outcome.control,
            HookResultControl::Block { .. }
        ));
        assert_eq!(open_outcome.control, HookResultControl::Continue);
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
