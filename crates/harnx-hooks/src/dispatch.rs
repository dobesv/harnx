use crate::{
    executor::execute_command_hook, AsyncHookManager, CompiledMatcher, HookCommand, HookEvent,
    HookOutcome, HookPayload, HookResult, HookResultControl, PersistentHookManager,
};

use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;

/// Inline hook metadata used by sandbox-run's non-NATS dispatcher.
#[derive(Debug, Clone)]
pub struct InlineHookSpec {
    pub event: String,
    pub matcher: Option<String>,
    pub command: HookCommand,
    pub async_hook: Option<bool>,
}

#[derive(Clone, Copy, Default)]
pub struct DispatchOptions<'a> {
    pub persistent_modes: &'a [bool],
    pub resume_count: u32,
    pub async_manager: Option<&'a AsyncHookManager>,
    pub persistent_manager: Option<&'a Arc<TokioMutex<PersistentHookManager>>>,
}

pub async fn dispatch_hooks(
    event: &HookEvent,
    hooks: &[InlineHookSpec],
    session_id: &str,
    cwd: &Path,
) -> HookOutcome {
    dispatch_hooks_with_options(event, hooks, session_id, cwd, DispatchOptions::default()).await
}

struct DispatchState {
    payload: HookPayload,
    additional_contexts: Vec<String>,
    resume: bool,
    current_tool_input: Option<Value>,
    current_tool_response: Option<Value>,
}

impl DispatchState {
    fn new(event: &HookEvent, session_id: &str, cwd: &Path, resume_count: u32) -> Self {
        Self {
            payload: HookPayload {
                session_id: session_id.to_string(),
                cwd: cwd.to_path_buf(),
                resume_count,
                hook_event: event.clone(),
            },
            additional_contexts: Vec::new(),
            resume: false,
            current_tool_input: None,
            current_tool_response: None,
        }
    }

    fn patch_payload(&mut self) {
        patch_payload_mutation(
            &mut self.payload.hook_event,
            self.current_tool_input.as_ref(),
            self.current_tool_response.as_ref(),
        );
    }

    fn apply_outcome(&mut self, outcome: HookOutcome) -> Option<HookOutcome> {
        let HookOutcome { control, result } = outcome;
        self.capture_mutations(&result);
        match control {
            HookResultControl::Continue => {
                self.capture_context(result);
                None
            }
            control => Some(HookOutcome {
                control,
                result: HookResult {
                    mutated_tool_input: self.current_tool_input.clone(),
                    mutated_tool_response: self.current_tool_response.clone(),
                    ..result
                },
            }),
        }
    }

    fn capture_mutations(&mut self, result: &HookResult) {
        let Some(output) = &result.hook_specific_output else {
            return;
        };
        if let Some(tool_input) = &output.tool_input {
            self.current_tool_input = Some(tool_input.clone());
        }
        if let Some(tool_response) = &output.tool_response {
            self.current_tool_response = Some(tool_response.clone());
        }
    }

    fn capture_context(&mut self, result: HookResult) {
        self.additional_contexts.extend(
            [result.additional_context, result.system_message]
                .into_iter()
                .flatten()
                .filter(|value| !value.is_empty()),
        );
        self.resume |= result.resume.unwrap_or(false);
    }

    fn finish(self) -> HookOutcome {
        HookOutcome {
            control: HookResultControl::Continue,
            result: HookResult {
                additional_context: (!self.additional_contexts.is_empty())
                    .then(|| self.additional_contexts.join("\n")),
                resume: self.resume.then_some(true),
                mutated_tool_input: self.current_tool_input,
                mutated_tool_response: self.current_tool_response,
                ..HookResult::default()
            },
        }
    }
}

pub async fn dispatch_hooks_with_options(
    event: &HookEvent,
    hooks: &[InlineHookSpec],
    session_id: &str,
    cwd: &Path,
    options: DispatchOptions<'_>,
) -> HookOutcome {
    let mut state = DispatchState::new(event, session_id, cwd, options.resume_count);
    for (index, hook) in hooks.iter().enumerate() {
        if !hook_matches_event(hook, event) {
            continue;
        }
        state.patch_payload();
        let Some(outcome) = execute_hook(index, hook, &state.payload, options).await else {
            continue;
        };
        if let Some(terminal) = state.apply_outcome(outcome) {
            return terminal;
        }
    }
    state.finish()
}

fn hook_matches_event(hook: &InlineHookSpec, event: &HookEvent) -> bool {
    if hook.event != event.event_name() {
        return false;
    }
    match CompiledMatcher::compile(&hook.matcher) {
        Ok(matcher) => matcher.matches(event),
        Err(error) => {
            warn!(
                "Skipping hook `{}` for event `{}` because matcher compilation failed: {error}",
                hook.command.command, hook.event
            );
            false
        }
    }
}

async fn execute_hook(
    index: usize,
    hook: &InlineHookSpec,
    payload: &HookPayload,
    options: DispatchOptions<'_>,
) -> Option<HookOutcome> {
    if hook.async_hook == Some(true) {
        if let Some(manager) = options.async_manager {
            manager.spawn_hook(payload.clone(), hook.command.clone());
        }
        return None;
    }
    if options
        .persistent_modes
        .get(index)
        .copied()
        .unwrap_or(false)
    {
        let manager = options.persistent_manager?;
        return Some(
            manager
                .lock()
                .await
                .send_event(payload, &hook.command)
                .await,
        );
    }
    Some(execute_command_hook(payload, &hook.command).await)
}

fn patch_payload_mutation(
    event: &mut HookEvent,
    tool_input: Option<&Value>,
    tool_response: Option<&Value>,
) {
    match event {
        HookEvent::PreToolUse {
            tool_input: payload_tool_input,
            ..
        } => {
            if let Some(tool_input) = tool_input {
                *payload_tool_input = tool_input.clone();
            }
        }
        HookEvent::PostToolUse {
            tool_input: payload_tool_input,
            tool_response: payload_tool_response,
            ..
        } => {
            if let Some(tool_input) = tool_input {
                *payload_tool_input = tool_input.clone();
            }
            if let Some(tool_response) = tool_response {
                *payload_tool_response = tool_response.clone();
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{dispatch_hooks, dispatch_hooks_with_options, DispatchOptions, InlineHookSpec};
    use crate::{AsyncHookManager, HookCommand, HookEvent, HookResultControl};
    use serde_json::json;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    #[cfg(unix)]
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(unix)]
    static SCRIPT_COUNTER: AtomicU64 = AtomicU64::new(1);

    fn temp_test_dir(name: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("unix epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("harnx-dispatch-tests-{name}-{suffix}"));
        fs::create_dir_all(&dir).expect("create temp dispatch dir");
        dir
    }

    #[cfg(unix)]
    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', r#"'\''"#))
    }

    #[cfg(windows)]
    fn powershell_quote(value: &str) -> String {
        value.replace('\'', "''")
    }

    #[cfg(windows)]
    fn encode_powershell_script(script: &str) -> String {
        let utf16: Vec<u8> = script
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        harnx_core::crypto::base64_encode(utf16)
    }

    #[cfg(unix)]
    fn write_script(dir: &Path, name: &str, body: &str) -> String {
        let id = SCRIPT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("{name}-{id}.sh"));
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).expect("write shell script");

        let mut permissions = fs::metadata(&path).expect("script metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("set shell script permissions");

        shell_quote(&path.display().to_string())
    }

    #[cfg(windows)]
    fn write_script(_dir: &Path, _name: &str, body: &str) -> String {
        let wrapped = format!("$ProgressPreference = 'SilentlyContinue'\n{body}");
        let encoded = encode_powershell_script(&wrapped);
        format!("powershell.exe -NoProfile -ExecutionPolicy Bypass -EncodedCommand {encoded}")
    }

    fn pre_tool_use_event(tool_name: &str) -> HookEvent {
        HookEvent::PreToolUse {
            tool_name: tool_name.to_string(),
            tool_input: json!({"command": "pwd"}),
            tool_use_id: "call-1".to_string(),
        }
    }

    fn hook_config(event: &str, command: String) -> InlineHookSpec {
        InlineHookSpec {
            event: event.to_string(),
            matcher: None,
            command: HookCommand {
                command,
                timeout: Some(5),
                package_dir: None,
            },
            async_hook: None,
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
    fn write_line_command(dir: &Path, path: &Path, line: &str) -> String {
        write_script(
            dir,
            "write-line",
            &format!(
                "printf '%s\\n' {} >> {}",
                shell_quote(line),
                shell_quote(&path.display().to_string())
            ),
        )
    }

    #[cfg(windows)]
    fn write_line_command(dir: &Path, path: &Path, line: &str) -> String {
        write_script(
            dir,
            "write-line",
            &format!(
                "Add-Content -Path '{}' -Value '{}'\n",
                powershell_quote(&path.display().to_string()),
                powershell_quote(line)
            ),
        )
    }

    #[cfg(unix)]
    fn block_command(dir: &Path, path: &Path) -> String {
        write_script(
            dir,
            "block",
            &format!(
                "printf '%s\\n' {} > {}\necho {} >&2\nexit 2",
                shell_quote("blocked"),
                shell_quote(&path.display().to_string()),
                shell_quote("blocked")
            ),
        )
    }

    #[cfg(windows)]
    fn block_command(dir: &Path, path: &Path) -> String {
        write_script(
            dir,
            "block",
            &format!(
                "Set-Content -Path '{}' -Value 'blocked'\n[Console]::Error.WriteLine('blocked')\nexit 2\n",
                powershell_quote(&path.display().to_string())
            ),
        )
    }

    #[cfg(unix)]
    fn ask_json_command(dir: &Path, reason: &str) -> String {
        let output = json!({
            "hookSpecificOutput": {
                "permissionDecision": "ask",
                "permissionDecisionReason": reason,
            }
        })
        .to_string();
        write_script(
            dir,
            "ask-json",
            &format!("printf '%s\\n' {}", shell_quote(&output)),
        )
    }

    #[cfg(windows)]
    fn ask_json_command(dir: &Path, reason: &str) -> String {
        write_script(
            dir,
            "ask-json",
            &format!(
                "$output = @{{ hookSpecificOutput = @{{ permissionDecision = 'ask'; permissionDecisionReason = '{}' }} }} | ConvertTo-Json -Compress\n[Console]::Out.WriteLine($output)\n",
                powershell_quote(reason)
            ),
        )
    }

    #[cfg(unix)]
    fn allow_json_command(dir: &Path) -> String {
        let output = json!({
            "hookSpecificOutput": {
                "permissionDecision": "allow",
            }
        })
        .to_string();
        write_script(
            dir,
            "allow-json",
            &format!("printf '%s\\n' {}", shell_quote(&output)),
        )
    }

    #[cfg(windows)]
    fn allow_json_command(dir: &Path) -> String {
        write_script(
            dir,
            "allow-json",
            "$output = @{ hookSpecificOutput = @{ permissionDecision = 'allow' } } | ConvertTo-Json -Compress\n[Console]::Out.WriteLine($output)\n",
        )
    }

    #[cfg(unix)]
    fn tool_input_mutation_command(dir: &Path, value: serde_json::Value) -> String {
        let output = json!({
            "hookSpecificOutput": {
                "toolInput": value,
            }
        })
        .to_string();
        write_script(
            dir,
            "tool-input-mutation",
            &format!("printf '%s\n' {}", shell_quote(&output)),
        )
    }

    #[cfg(unix)]
    fn tool_response_mutation_command(dir: &Path, value: serde_json::Value) -> String {
        let output = json!({
            "hookSpecificOutput": {
                "toolResponse": value,
            }
        })
        .to_string();
        write_script(
            dir,
            "tool-response-mutation",
            &format!("printf '%s\n' {}", shell_quote(&output)),
        )
    }

    #[cfg(unix)]
    fn dump_and_mutate_tool_input_command(
        dir: &Path,
        path: &Path,
        value: serde_json::Value,
    ) -> String {
        let output = json!({
            "hookSpecificOutput": {
                "toolInput": value,
            }
        })
        .to_string();
        write_script(
            dir,
            "dump-and-mutate-tool-input",
            &format!(
                "cat > {path}\nprintf '%s\n' {output}",
                path = shell_quote(&path.display().to_string()),
                output = shell_quote(&output),
            ),
        )
    }

    #[cfg(unix)]
    fn invalid_json_command(dir: &Path) -> String {
        write_script(dir, "invalid-json", "printf '%s\n' '{not json}'")
    }

    #[cfg(unix)]
    fn payload_dump_command(dir: &Path, path: &Path) -> String {
        write_script(
            dir,
            "payload-dump",
            &format!("cat > {}", shell_quote(&path.display().to_string())),
        )
    }

    #[cfg(windows)]
    fn payload_dump_command(dir: &Path, path: &Path) -> String {
        write_script(
            dir,
            "payload-dump",
            &format!(
                "$content = [Console]::In.ReadToEnd()\n[System.IO.File]::WriteAllText('{}', $content)\n",
                powershell_quote(&path.display().to_string())
            ),
        )
    }

    #[tokio::test]
    async fn test_dispatch_filters_by_event() {
        let cwd = temp_test_dir("filter-by-event");
        let marker = cwd.join("hook-runs.txt");
        let hooks = vec![
            hook_config("PreToolUse", write_line_command(&cwd, &marker, "pre-tool")),
            hook_config(
                "SessionStart",
                write_line_command(&cwd, &marker, "session-start"),
            ),
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
            hook_config("PreToolUse", block_command(&cwd, &blocked_marker)),
            hook_config(
                "PreToolUse",
                write_line_command(&cwd, &second_marker, "second"),
            ),
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
            hook_config("PreToolUse", ask_json_command(&cwd, "confirm this")),
            hook_config(
                "PreToolUse",
                write_line_command(&cwd, &second_marker, "second"),
            ),
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

    #[cfg(unix)]
    #[tokio::test]
    async fn test_dispatch_pre_tool_mutation_single_hook() {
        let cwd = temp_test_dir("pre-tool-mutation-single");
        let hooks = vec![hook_config(
            "PreToolUse",
            tool_input_mutation_command(&cwd, json!({"mutated": true})),
        )];

        let outcome = dispatch_hooks(
            &pre_tool_use_event("shell"),
            &hooks,
            "session-mutate-1",
            &cwd,
        )
        .await;

        assert!(matches!(outcome.control, HookResultControl::Continue));
        assert_eq!(
            outcome.result.mutated_tool_input,
            Some(json!({"mutated": true}))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_dispatch_pre_tool_mutation_chains_between_hooks() {
        let cwd = temp_test_dir("pre-tool-mutation-chain");
        let payload_dump = cwd.join("second-payload.json");
        let hooks = vec![
            hook_config(
                "PreToolUse",
                tool_input_mutation_command(&cwd, json!({"step": 1})),
            ),
            hook_config(
                "PreToolUse",
                dump_and_mutate_tool_input_command(&cwd, &payload_dump, json!({"step": 2})),
            ),
        ];

        let outcome = dispatch_hooks(
            &pre_tool_use_event("shell"),
            &hooks,
            "session-mutate-2",
            &cwd,
        )
        .await;

        assert!(matches!(outcome.control, HookResultControl::Continue));
        let payload: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(&payload_dump).expect("read chained payload dump"),
        )
        .expect("parse chained payload dump");
        assert_eq!(payload["tool_input"], json!({"step": 1}));
        assert_eq!(outcome.result.mutated_tool_input, Some(json!({"step": 2})));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_dispatch_post_tool_mutation_single_hook() {
        let cwd = temp_test_dir("post-tool-mutation-single");
        let event = HookEvent::PostToolUse {
            tool_name: "shell".to_string(),
            tool_input: json!({"command": "pwd"}),
            tool_use_id: "call-post-1".to_string(),
            tool_response: json!({"ok": false}),
        };
        let hooks = vec![hook_config(
            "PostToolUse",
            tool_response_mutation_command(&cwd, json!({"ok": true})),
        )];

        let outcome = dispatch_hooks(&event, &hooks, "session-post-1", &cwd).await;

        assert!(matches!(outcome.control, HookResultControl::Continue));
        assert_eq!(
            outcome.result.mutated_tool_response,
            Some(json!({"ok": true}))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_dispatch_block_wins_over_mutation() {
        let cwd = temp_test_dir("block-over-mutation");
        let blocked_marker = cwd.join("blocked.txt");
        let hooks = vec![
            hook_config(
                "PreToolUse",
                tool_input_mutation_command(&cwd, json!({"step": 1})),
            ),
            hook_config("PreToolUse", block_command(&cwd, &blocked_marker)),
        ];

        let outcome = dispatch_hooks(
            &pre_tool_use_event("shell"),
            &hooks,
            "session-block-1",
            &cwd,
        )
        .await;

        match outcome.control {
            HookResultControl::Block { reason } => assert_eq!(reason, "blocked"),
            HookResultControl::Ask { .. } => panic!("expected blocked hook outcome, got ask"),
            HookResultControl::Continue => panic!("expected blocked hook outcome"),
        }
        assert!(blocked_marker.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_dispatch_ask_preserves_prior_tool_input_mutation() {
        let cwd = temp_test_dir("ask-preserves-mutation");
        let hooks = vec![
            hook_config(
                "PreToolUse",
                tool_input_mutation_command(&cwd, json!({"mutated": true})),
            ),
            hook_config("PreToolUse", ask_json_command(&cwd, "confirm this")),
        ];

        let outcome = dispatch_hooks(
            &pre_tool_use_event("shell"),
            &hooks,
            "session-ask-mutation",
            &cwd,
        )
        .await;

        match outcome.control {
            HookResultControl::Ask { reason } => {
                assert_eq!(reason.as_deref(), Some("confirm this"));
            }
            HookResultControl::Block { .. } => panic!("expected ask hook outcome, got block"),
            HookResultControl::Continue => panic!("expected ask hook outcome"),
        }
        assert_eq!(
            outcome.result.mutated_tool_input,
            Some(json!({"mutated": true}))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_dispatch_block_preserves_prior_tool_input_mutation() {
        let cwd = temp_test_dir("block-preserves-mutation");
        let hooks = vec![
            hook_config(
                "PreToolUse",
                tool_input_mutation_command(&cwd, json!({"mutated": true})),
            ),
            hook_config(
                "PreToolUse",
                block_command(&cwd, &cwd.join("block-marker.txt")),
            ),
        ];

        let outcome = dispatch_hooks(
            &pre_tool_use_event("shell"),
            &hooks,
            "session-block-mutation",
            &cwd,
        )
        .await;

        match outcome.control {
            HookResultControl::Block { reason } => assert_eq!(reason, "blocked"),
            HookResultControl::Ask { .. } => panic!("expected blocked hook outcome, got ask"),
            HookResultControl::Continue => panic!("expected blocked hook outcome"),
        }
        assert_eq!(
            outcome.result.mutated_tool_input,
            Some(json!({"mutated": true}))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_dispatch_bad_json_yields_no_mutation() {
        let cwd = temp_test_dir("bad-json-no-mutation");
        let hooks = vec![hook_config("PreToolUse", invalid_json_command(&cwd))];

        let outcome = dispatch_hooks(
            &pre_tool_use_event("shell"),
            &hooks,
            "session-bad-json",
            &cwd,
        )
        .await;

        assert!(matches!(outcome.control, HookResultControl::Continue));
        assert!(outcome.result.mutated_tool_input.is_none());
        assert_eq!(
            outcome.result.additional_context.as_deref(),
            Some("{not json}")
        );
    }

    #[tokio::test]
    async fn test_dispatch_ask_explicit_allow() {
        let cwd = temp_test_dir("ask-explicit-allow");
        let hooks = vec![hook_config("PreToolUse", allow_json_command(&cwd))];

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
            payload_dump_command(&cwd, &marker),
        )];

        let outcome = dispatch_hooks_with_options(
            &pre_tool_use_event("shell"),
            &hooks,
            "session-3",
            &cwd,
            DispatchOptions {
                resume_count: 4,
                ..DispatchOptions::default()
            },
        )
        .await;

        assert!(matches!(outcome.control, HookResultControl::Continue));
        let payload = fs::read_to_string(&marker).expect("read payload marker");
        assert!(payload.contains("\"resume_count\":4"));
    }

    #[tokio::test]
    async fn test_dispatch_async_hook_does_not_block() {
        let cwd = temp_test_dir("async-no-block");
        let hooks = vec![InlineHookSpec {
            async_hook: Some(true),
            ..hook_config("PreToolUse", sleep_command(5))
        }];
        let manager = AsyncHookManager::new();
        let start = tokio::time::Instant::now();

        let outcome = dispatch_hooks_with_options(
            &pre_tool_use_event("shell"),
            &hooks,
            "session-async",
            &cwd,
            DispatchOptions {
                async_manager: Some(&manager),
                ..DispatchOptions::default()
            },
        )
        .await;

        assert!(matches!(outcome.control, HookResultControl::Continue));
        assert!(start.elapsed() < Duration::from_secs(1));

        tokio::time::sleep(Duration::from_millis(1200)).await;
    }

    // A persistent hook script that reads JSONL lines, echoes back the id, and
    // injects {"injected": true} into hookSpecificOutput.toolInput.
    #[cfg(unix)]
    fn persistent_mutate_command(dir: &Path) -> String {
        write_script(
            dir,
            "persistent-mutate",
            r#"while IFS= read -r line; do
  id=${line#*\"id\":\"}; id=${id%%\"*}
  printf '{"id":"%s","hookSpecificOutput":{"toolInput":{"injected":true}}}\n' "$id"
done"#,
        )
    }

    // Regression test: persistent hooks were silently skipped when
    // persistent_manager was None (the bug in build_tool_eval_context).
    // Passing None must produce no mutation; passing Some(pm) must mutate.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_dispatch_persistent_hook_requires_manager() {
        use crate::PersistentHookManager;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let cwd = temp_test_dir("persistent-hook-manager");
        let command = persistent_mutate_command(&cwd);
        let hooks = vec![hook_config("PreToolUse", command)];
        let event = pre_tool_use_event("bash_exec");

        // With None: persistent hook is skipped — no mutation.
        let outcome_no_pm = dispatch_hooks_with_options(
            &event,
            &hooks,
            "session-no-pm",
            &cwd,
            DispatchOptions {
                persistent_modes: &[true],
                ..DispatchOptions::default()
            },
        )
        .await;
        assert!(matches!(outcome_no_pm.control, HookResultControl::Continue));
        assert!(
            outcome_no_pm.result.mutated_tool_input.is_none(),
            "persistent hook must be skipped when no manager is passed"
        );

        // With Some(pm): persistent hook runs and mutates tool_input.
        let pm = Arc::new(Mutex::new(PersistentHookManager::new()));
        let outcome_with_pm = dispatch_hooks_with_options(
            &event,
            &hooks,
            "session-with-pm",
            &cwd,
            DispatchOptions {
                persistent_modes: &[true],
                persistent_manager: Some(&pm),
                ..DispatchOptions::default()
            },
        )
        .await;
        assert!(matches!(
            outcome_with_pm.control,
            HookResultControl::Continue
        ));
        assert_eq!(
            outcome_with_pm.result.mutated_tool_input,
            Some(serde_json::json!({"injected": true})),
            "persistent hook must mutate tool_input when manager is provided"
        );
        pm.lock().await.shutdown();
    }
}
