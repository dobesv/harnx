use crate::config::Input;
use crate::hooks::{executor::execute_command_hook, HookPayload, HookResult};

use tokio::sync::mpsc;

pub struct AsyncHookManager {
    sender: mpsc::UnboundedSender<HookResult>,
    receiver: mpsc::UnboundedReceiver<HookResult>,
}

impl Default for AsyncHookManager {
    fn default() -> Self {
        Self::new()
    }
}

impl AsyncHookManager {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self { sender, receiver }
    }

    pub fn spawn_hook(&self, payload: HookPayload, command: String, timeout: Option<u64>) {
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let outcome = execute_command_hook(&payload, &command, timeout).await;
            let _ = sender.send(outcome.result);
        });
    }

    pub fn drain_pending(&mut self) -> Option<HookResult> {
        let mut contexts = vec![];
        let mut resume = false;

        loop {
            match self.receiver.try_recv() {
                Ok(result) => {
                    if let Some(ctx) = result.additional_context.filter(|s| !s.is_empty()) {
                        contexts.push(ctx);
                    }
                    if let Some(msg) = result.system_message.filter(|s| !s.is_empty()) {
                        contexts.push(msg);
                    }
                    resume |= result.resume.unwrap_or(false);
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }

        if contexts.is_empty() && !resume {
            None
        } else {
            Some(HookResult {
                additional_context: (!contexts.is_empty()).then(|| contexts.join("\n")),
                resume: resume.then_some(true),
                ..HookResult::default()
            })
        }
    }
}

pub fn append_pending_context(pending_async_context: &mut Option<String>, context: String) {
    if context.is_empty() {
        return;
    }

    match pending_async_context {
        Some(existing) if !existing.is_empty() => {
            existing.push_str("\n\n");
            existing.push_str(&context);
        }
        _ => *pending_async_context = Some(context),
    }
}

pub fn drain_async_results(
    async_manager: &mut AsyncHookManager,
    pending_async_context: &mut Option<String>,
) -> bool {
    let mut resume = false;
    if let Some(pending) = async_manager.drain_pending() {
        if let Some(context) = pending.additional_context.filter(|value| !value.is_empty()) {
            append_pending_context(pending_async_context, context);
        }
        resume = pending.resume.unwrap_or(false);
    }
    resume
}

pub fn inject_pending_async_context(input: &mut Input, pending_async_context: &mut Option<String>) {
    let Some(context) = pending_async_context
        .take()
        .filter(|value| !value.is_empty())
    else {
        return;
    };

    let input_text = input.text();
    input.clear_patch();
    input.set_text(if input_text.is_empty() {
        context
    } else {
        format!("{context}\n\n{input_text}")
    });
}

#[cfg(test)]
mod tests {
    use super::AsyncHookManager;
    use crate::hooks::{HookEvent, HookPayload};
    use serde_json::json;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::time::{sleep, Duration};

    fn test_payload(cwd: &Path) -> HookPayload {
        HookPayload {
            session_id: "session-123".to_string(),
            cwd: cwd.to_path_buf(),
            resume_count: 0,
            hook_event: HookEvent::PreToolUse {
                tool_name: "shell".to_string(),
                tool_input: json!({"command": "pwd"}),
                tool_use_id: "call-1".to_string(),
            },
        }
    }

    fn temp_test_dir(name: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("unix epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("harnx-async-hook-tests-{name}-{suffix}"));
        fs::create_dir_all(&dir).expect("create temp test dir");
        dir
    }

    #[cfg(unix)]
    fn echo_command() -> &'static str {
        "echo 'async hook complete'"
    }

    #[cfg(windows)]
    fn echo_command() -> &'static str {
        "echo async hook complete"
    }

    #[tokio::test]
    async fn test_async_manager_spawn_and_drain() {
        let cwd = temp_test_dir("spawn-and-drain");
        let payload = test_payload(&cwd);
        let mut manager = AsyncHookManager::new();

        manager.spawn_hook(payload, echo_command().to_string(), Some(5));
        sleep(Duration::from_millis(150)).await;

        let drained = manager.drain_pending().expect("expected async hook result");
        assert_eq!(
            drained.additional_context.as_deref(),
            Some("async hook complete")
        );
        assert!(drained.resume.is_none());
    }

    #[test]
    fn test_async_manager_drain_empty() {
        let mut manager = AsyncHookManager::new();
        assert!(manager.drain_pending().is_none());
    }
}
