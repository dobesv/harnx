use crate::hooks::{
    executor::execute_command_hook, AsyncHookManager, CompiledMatcher, HookConfig, HookEvent,
    HookOutcome, HookPayload, HookResult, HookResultControl, PersistentHookManager,
};

use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;

pub async fn dispatch_hooks(
    event: &HookEvent,
    hooks: &[HookConfig],
    session_id: &str,
    cwd: &Path,
) -> HookOutcome {
    dispatch_hooks_with_count(event, hooks, session_id, cwd, 0, None).await
}


pub async fn dispatch_hooks_with_managers(
    event: &HookEvent,
    hooks: &[HookConfig],
    session_id: &str,
    cwd: &Path,
    async_manager: Option<&AsyncHookManager>,
    persistent_manager: Option<&Arc<TokioMutex<PersistentHookManager>>>,
) -> HookOutcome {
    dispatch_hooks_with_count_and_manager(
        event,
        hooks,
        session_id,
        cwd,
        0,
        async_manager,
        persistent_manager,
    )
    .await
}

pub async fn dispatch_hooks_with_count(
    event: &HookEvent,
    hooks: &[HookConfig],
    session_id: &str,
    cwd: &Path,
    resume_count: u32,
    persistent_manager: Option<&Arc<TokioMutex<PersistentHookManager>>>,
) -> HookOutcome {
    dispatch_hooks_with_count_and_manager(
        event,
        hooks,
        session_id,
        cwd,
        resume_count,
        None,
        persistent_manager,
    )
    .await
}

pub async fn dispatch_hooks_with_count_and_manager(
    event: &HookEvent,
    hooks: &[HookConfig],
    session_id: &str,
    cwd: &Path,
    resume_count: u32,
    async_manager: Option<&AsyncHookManager>,
    persistent_manager: Option<&Arc<TokioMutex<PersistentHookManager>>>,
) -> HookOutcome {
    let payload = HookPayload {
        session_id: session_id.to_string(),
        cwd: cwd.to_path_buf(),
        resume_count,
        hook_event: event.clone(),
    };

    let mut additional_contexts = vec![];
    let mut resume = false;

    for hook in hooks {
        if hook.event != event.event_name() || !hook.is_supported_type() {
            continue;
        }

        let matcher = match CompiledMatcher::compile(&hook.matcher) {
            Ok(matcher) => matcher,
            Err(err) => {
                warn!(
                    "Skipping hook `{}` for event `{}` because matcher compilation failed: {err}",
                    hook.command, hook.event
                );
                continue;
            }
        };

        if !matcher.matches(event) {
            continue;
        }

        if hook.async_hook == Some(true) {
            if let Some(manager) = async_manager {
                manager.spawn_hook(payload.clone(), hook.command.clone(), hook.timeout);
            }
            continue;
        }

        if hook.hook_type == "claude-command-persistent" {
            if let Some(pm) = persistent_manager {
                let outcome = pm
                    .lock()
                    .await
                    .send_event(&hook.command, &payload, hook.timeout)
                    .await;
                let HookOutcome { control, result } = outcome;

                match control {
                    HookResultControl::Block { reason } => {
                        return HookOutcome {
                            control: HookResultControl::Block { reason },
                            result,
                        };
                    }
                    HookResultControl::Ask { reason } => {
                        return HookOutcome {
                            control: HookResultControl::Ask { reason },
                            result,
                        };
                    }
                    HookResultControl::Continue => {
                        if let Some(context) =
                            result.additional_context.filter(|value| !value.is_empty())
                        {
                            additional_contexts.push(context);
                        }
                        if let Some(msg) = result.system_message.filter(|value| !value.is_empty()) {
                            additional_contexts.push(msg);
                        }
                        resume |= result.resume.unwrap_or(false);
                    }
                }
            }
            continue;
        }

        let outcome = execute_command_hook(&payload, &hook.command, hook.timeout).await;
        let HookOutcome { control, result } = outcome;

        match control {
            HookResultControl::Block { reason } => {
                return HookOutcome {
                    control: HookResultControl::Block { reason },
                    result,
                };
            }
            HookResultControl::Ask { reason } => {
                return HookOutcome {
                    control: HookResultControl::Ask { reason },
                    result,
                };
            }
            HookResultControl::Continue => {
                if let Some(context) = result.additional_context.filter(|value| !value.is_empty()) {
                    additional_contexts.push(context);
                }
                if let Some(msg) = result.system_message.filter(|value| !value.is_empty()) {
                    additional_contexts.push(msg);
                }
                resume |= result.resume.unwrap_or(false);
            }
        }
    }

    HookOutcome {
        control: HookResultControl::Continue,
        result: HookResult {
            additional_context: (!additional_contexts.is_empty())
                .then(|| additional_contexts.join("\n")),
            resume: resume.then_some(true),
            ..HookResult::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{dispatch_hooks, dispatch_hooks_with_count, dispatch_hooks_with_count_and_manager};
    use crate::hooks::{AsyncHookManager, HookConfig, HookEvent, HookResultControl};
    use serde_json::json;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_test_dir(name: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("unix epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("harnx-dispatch-tests-{name}-{suffix}"));
        fs::create_dir_all(&dir).expect("create temp dispatch dir");
        dir
    }

    fn pre_tool_use_event(tool_name: &str) -> HookEvent {
        HookEvent::PreToolUse {
            tool_name: tool_name.to_string(),
            tool_input: json!({"command": "pwd"}),
            tool_use_id: "call-1".to_string(),
        }
    }

    fn hook_config(event: &str, command: String) -> HookConfig {
        HookConfig {
            event: event.to_string(),
            matcher: None,
            command,
            timeout: Some(5),
            status_message: None,
            async_hook: None,
            hook_type: "claude-command".to_string(),
        }
    }

    #[cfg(unix)]
    fn sleep_command(seconds: u64) -> String {
        format!("sleep {seconds}")
    }

    #[cfg(windows)]
    fn sleep_command(seconds: u64) -> String {
        format!("powershell -Command \"Start-Sleep -Seconds {seconds}\"")
    }

    #[cfg(unix)]
    fn write_line_command(path: &Path, line: &str) -> String {
        format!("printf '%s\\n' '{line}' >> '{}'", path.display())
    }

    #[cfg(windows)]
    fn write_line_command(path: &Path, line: &str) -> String {
        format!("echo {line}>>{}", path.display())
    }

    #[cfg(unix)]
    fn block_command(path: &Path) -> String {
        format!(
            "printf '%s\\n' 'blocked' > '{}' && echo 'blocked' >&2 && exit 2",
            path.display()
        )
    }

    #[cfg(windows)]
    fn block_command(path: &Path) -> String {
        format!(
            "echo blocked>{} && echo blocked 1>&2 && exit 2",
            path.display()
        )
    }

    #[cfg(unix)]
    fn ask_json_command(reason: &str) -> String {
        format!(
            "echo '{{\"hookSpecificOutput\":{{\"permissionDecision\":\"ask\",\"permissionDecisionReason\":\"{reason}\"}}}}'"
        )
    }

    #[cfg(windows)]
    fn ask_json_command(reason: &str) -> String {
        format!(
            "echo {{\"hookSpecificOutput\":{{\"permissionDecision\":\"ask\",\"permissionDecisionReason\":\"{reason}\"}}}}"
        )
    }

    #[cfg(unix)]
    fn allow_json_command() -> String {
        "echo '{\"hookSpecificOutput\":{\"permissionDecision\":\"allow\"}}'".to_string()
    }

    #[cfg(windows)]
    fn allow_json_command() -> String {
        "echo {\"hookSpecificOutput\":{\"permissionDecision\":\"allow\"}}".to_string()
    }

    #[tokio::test]
    async fn test_dispatch_filters_by_event() {
        let cwd = temp_test_dir("filter-by-event");
        let marker = cwd.join("hook-runs.txt");
        let hooks = vec![
            hook_config("PreToolUse", write_line_command(&marker, "pre-tool")),
            hook_config("SessionStart", write_line_command(&marker, "session-start")),
        ];

        let outcome = dispatch_hooks(&pre_tool_use_event("shell"), &hooks, "session-1", &cwd).await;

        assert!(matches!(outcome.control, HookResultControl::Continue));
        let contents = fs::read_to_string(&marker).expect("read marker file");
        assert_eq!(contents.trim(), "pre-tool");
    }

    #[tokio::test]
    async fn test_dispatch_block_short_circuit() {
        let cwd = temp_test_dir("block-short-circuit");
        let blocked_marker = cwd.join("blocked.txt");
        let second_marker = cwd.join("second.txt");
        let hooks = vec![
            hook_config("PreToolUse", block_command(&blocked_marker)),
            hook_config("PreToolUse", write_line_command(&second_marker, "second")),
        ];

        let outcome = dispatch_hooks(&pre_tool_use_event("shell"), &hooks, "session-2", &cwd).await;

        match outcome.control {
            HookResultControl::Block { reason } => assert_eq!(reason, "blocked"),
            HookResultControl::Ask { .. } => panic!("expected blocked hook outcome, got ask"),
            HookResultControl::Continue => panic!("expected blocked hook outcome"),
        }
        assert!(blocked_marker.exists());
        assert!(!second_marker.exists());
    }

    #[tokio::test]
    async fn test_dispatch_ask_short_circuit() {
        let cwd = temp_test_dir("ask-short-circuit");
        let second_marker = cwd.join("second.txt");
        let hooks = vec![
            hook_config("PreToolUse", ask_json_command("confirm this")),
            hook_config("PreToolUse", write_line_command(&second_marker, "second")),
        ];

        let outcome =
            dispatch_hooks(&pre_tool_use_event("shell"), &hooks, "session-ask", &cwd).await;

        match outcome.control {
            HookResultControl::Ask { reason } => {
                assert_eq!(reason.as_deref(), Some("confirm this"));
            }
            HookResultControl::Block { reason } => {
                panic!("expected ask hook outcome, got block: {reason}")
            }
            HookResultControl::Continue => panic!("expected ask hook outcome"),
        }
        assert!(!second_marker.exists());
    }

    #[tokio::test]
    async fn test_dispatch_ask_explicit_allow() {
        let cwd = temp_test_dir("ask-explicit-allow");
        let hooks = vec![hook_config("PreToolUse", allow_json_command())];

        let outcome =
            dispatch_hooks(&pre_tool_use_event("shell"), &hooks, "session-allow", &cwd).await;

        assert!(matches!(outcome.control, HookResultControl::Continue));
    }

    #[tokio::test]
    async fn test_dispatch_includes_resume_count_in_payload() {
        let cwd = temp_test_dir("resume-count");
        let marker = cwd.join("payload.json");
        let hooks = vec![hook_config(
            "PreToolUse",
            format!("python -c \"import sys, pathlib; pathlib.Path(r'{}').write_text(sys.stdin.read())\"", marker.display()),
        )];

        let outcome = dispatch_hooks_with_count(
            &pre_tool_use_event("shell"),
            &hooks,
            "session-3",
            &cwd,
            4,
            None,
        )
        .await;

        assert!(matches!(outcome.control, HookResultControl::Continue));
        let payload = fs::read_to_string(&marker).expect("read payload marker");
        assert!(payload.contains("\"resume_count\":4"));
    }

    #[tokio::test]
    async fn test_dispatch_async_hook_does_not_block() {
        let cwd = temp_test_dir("async-no-block");
        let hooks = vec![HookConfig {
            async_hook: Some(true),
            command: sleep_command(5),
            ..hook_config("PreToolUse", sleep_command(5))
        }];
        let manager = AsyncHookManager::new();
        let start = tokio::time::Instant::now();

        let outcome = dispatch_hooks_with_count_and_manager(
            &pre_tool_use_event("shell"),
            &hooks,
            "session-async",
            &cwd,
            0,
            Some(&manager),
            None,
        )
        .await;

        assert!(matches!(outcome.control, HookResultControl::Continue));
        assert!(start.elapsed() < Duration::from_secs(1));

        tokio::time::sleep(Duration::from_millis(1200)).await;
    }
}
