//! Foreground templates own their process: a new message must reap it, not detach it.
mod inherited_pipes;

use super::*;
use crate::{tool_template::ToolTemplate, BashToolset};
use harnx_toolset::{ToolInvocation, ToolInvocationContext, Toolset};
use tokio_util::sync::CancellationToken;

fn template(policy: &str, script: &str) -> ToolTemplate {
    serde_yaml::from_str(&format!(
        "name: test_template\ndescription: Test template\n{policy}\nsandbox: {{enabled: false}}\nscript: {}\n",
        serde_json::to_string(script).unwrap()
    ))
    .unwrap()
}

fn toolset(directory: &TestDir, template: ToolTemplate) -> BashToolset {
    // These unsandboxed fixtures need only a cwd grant. System grants can make
    // the default cwd /Library on macOS, whose history discovery exceeds our bounds.
    let mut allowlist = ResolvedAllowlist::new();
    assert!(allowlist.insert_rwx(directory.path()));
    let server = BashServer::new_with_templates(
        SandboxConfig {
            enabled: false,
            allowlist: Arc::new(allowlist),
            extra_env_passthrough: vec![],
            env_overrides: vec![],
            sandbox_run_path: PathBuf::from("harnx-sandbox-exec"),
        },
        vec![template],
    )
    .unwrap();
    BashToolset::from_server(server)
}

fn invocation(tool: &str, args: Value, token: &CancellationToken) -> ToolInvocation {
    ToolInvocation {
        tool: tool.into(),
        args,
        context: ToolInvocationContext {
            new_message: token.clone(),
            ..Default::default()
        },
        cancel: CancellationToken::new(),
    }
}

fn result_text(result: &Value) -> &str {
    assert_ne!(result["isError"], true, "{result}");
    result["content"][0]["text"].as_str().unwrap()
}

#[test]
fn template_on_user_message_spec_and_description_are_opt_in() {
    let directory = TestDir::new();
    for (policy, expected) in [
        ("", false),
        ("on_user_message: ignore", false),
        ("on_user_message: terminate", true),
    ] {
        let toolset = toolset(&directory, template(policy, "echo test"));
        let spec = toolset
            .tools()
            .into_iter()
            .find(|spec| spec.name == "test_template")
            .unwrap();
        assert_eq!(spec.stops_on_new_message(), expected);
        assert_eq!(
            spec.description.contains("stopped_by: new_message"),
            expected
        );
        assert!(spec.description.starts_with("Test template"));
        if expected {
            assert!(spec.description.starts_with("Test template. A new message"));
        }
        assert_eq!(spec.timeout_secs, Some(0));
        assert!(spec.meta.unwrap().contains_key("call_template"));
    }
}

async fn wait_for_pids(path: &Path) -> Vec<i32> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let text = tokio::fs::read_to_string(path).await.unwrap_or_default();
            if let Some(pids) = parse_pids(&text) {
                return pids;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("template must start and record its shell and child PIDs")
}

fn parse_pids(text: &str) -> Option<Vec<i32>> {
    let text = text.strip_suffix('\n')?;
    let pids: Vec<i32> = text
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    (pids.len() == 2 && pids.iter().all(|pid| *pid > 0)).then_some(pids)
}

#[test]
fn pid_records_require_complete_positive_pairs() {
    for text in ["", "1 2", "1\n", "1 nope\n", "1 2 3\n", "0 2\n", "-1 2\n"] {
        assert_eq!(parse_pids(text), None, "{text:?}");
    }
    assert_eq!(parse_pids("1 2\n"), Some(vec![1, 2]));
}

fn assert_reaped(pids: &[i32]) {
    for pid in pids {
        // PIDs were captured from this test's live child, not persisted state.
        let result = unsafe { libc::kill(*pid, 0) };
        assert_eq!(result, -1, "process {pid} survived template interruption");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}

#[tokio::test]
async fn terminate_action_stops_group_and_returns_partial_output() {
    let directory = TestDir::new();
    let pids_file = directory.path().join("pids");
    let mut template = template(
        "on_user_message: terminate",
        "sleep 30 &\nchild=$!\ntrap 'wait \"$child\"; exit 0' TERM\necho partial-stdout\necho partial-stderr >&2\nprintf '%s %s\\n' \"$$\" \"$child\" > \"$PID_FILE\"\nwait \"$child\"\necho unexpected-completion",
    );
    template.env = Some(HashMap::from([(
        "PID_FILE".into(),
        pids_file.to_string_lossy().into_owned(),
    )]));
    let toolset = toolset(&directory, template);
    let token = CancellationToken::new();
    let call = invocation("test_template", json!({}), &token);
    let task = tokio::spawn(async move { toolset.invoke_with_context(call).await });
    let pids = wait_for_pids(&pids_file).await;
    token.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("new message should stop a foreground template promptly")
        .unwrap()
        .unwrap();
    let text = result_text(&result);
    assert_eq!(extract_field(text, "status"), "interrupted", "{text}");
    assert_eq!(extract_field(text, "stopped_by"), "new_message", "{text}");
    assert!(text.contains("partial-stdout\n"), "{text}");
    assert!(text.contains("partial-stderr\n"), "{text}");
    assert!(!text.contains("\nunexpected-completion\n"), "{text}");
    assert_eq!(extract_field(text, "total_lines"), "2", "{text}");
    for stream in ["stdout", "stderr"] {
        let path = extract_field(text, &format!("{stream}_log_path"));
        let log = tokio::fs::read_to_string(path).await.unwrap();
        assert_eq!(log, format!("partial-{stream}\n"));
    }
    assert_reaped(&pids);
}

#[tokio::test]
async fn terminate_action_stops_for_already_pending_new_message() {
    let directory = TestDir::new();
    let toolset = toolset(
        &directory,
        template("on_user_message: terminate", "sleep 30"),
    );
    let token = CancellationToken::new();
    token.cancel();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        toolset.invoke_with_context(invocation("test_template", json!({}), &token)),
    )
    .await
    .expect("already-pending message should stop template promptly")
    .unwrap();
    let text = result_text(&result);
    assert_eq!(extract_field(text, "status"), "interrupted", "{text}");
    assert_eq!(extract_field(text, "stopped_by"), "new_message", "{text}");
}

#[tokio::test]
async fn ignore_action_and_exec_ignore_new_messages() {
    let directory = TestDir::new();
    for (tool, policy) in [
        ("test_template", ""),
        ("test_template", "on_user_message: ignore"),
        ("exec", ""),
    ] {
        let token = CancellationToken::new();
        token.cancel();
        let command = "sleep 0.2; echo completed";
        let toolset = toolset(&directory, template(policy, command));
        let args = if tool == "exec" {
            json!({"command": command, "working_dir": directory.path()})
        } else {
            json!({})
        };
        let result = toolset
            .invoke_with_context(invocation(tool, args, &token))
            .await
            .unwrap();
        let text = result_text(&result);
        assert_eq!(extract_field(text, "status"), "exited", "{text}");
        assert_eq!(extract_field(text, "exit_code"), "0", "{text}");
        assert!(!text.contains("stopped_by:"), "{text}");
        assert!(text.contains("\ncompleted\n"), "{text}");
    }
}

#[tokio::test]
async fn terminate_action_without_new_message_keeps_exit_and_timeout_behavior() {
    let directory = TestDir::new();
    let token = CancellationToken::new();
    for exit_code in [0, 7] {
        let toolset = toolset(
            &directory,
            template(
                "on_user_message: terminate",
                &format!("echo completed; exit {exit_code}"),
            ),
        );
        let result = toolset
            .invoke_with_context(invocation("test_template", json!({}), &token))
            .await
            .unwrap();
        let text = result_text(&result);
        assert_eq!(extract_field(text, "status"), "exited", "{text}");
        assert_eq!(
            extract_field(text, "exit_code"),
            exit_code.to_string(),
            "{text}"
        );
        assert!(!text.contains("stopped_by:"), "{text}");
    }
    let toolset = toolset(
        &directory,
        template("on_user_message: terminate", "sleep 30"),
    );
    let result = toolset
        .invoke_with_context(invocation(
            "test_template",
            json!({"timeout_secs": 1}),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(result["isError"], true, "{result}");
    assert!(
        result.to_string().contains("timed out after 1s"),
        "{result}"
    );
    assert!(!result.to_string().contains("stopped_by:"), "{result}");
}
