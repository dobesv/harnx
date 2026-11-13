use super::*;

async fn process_state(pid: i32) -> Option<String> {
    let output = tokio::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .await
        .expect("ps must be available on Unix test hosts");
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

async fn wait_for_shell_exit(pid: i32) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if process_state(pid)
                .await
                .is_none_or(|state| state.starts_with('Z'))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("shell must exit while its descendant still holds stderr open");
}

#[tokio::test]
async fn new_message_stops_descendant_holding_pipe_after_shell_exit() {
    let directory = TestDir::new();
    let pids_file = directory.path().join("pids");
    let mut template = template(
        "on_user_message: terminate",
        // The descendant closes stdout but holds stderr. Collecting stdout
        // before interruption must not lose it or poll its JoinHandle twice.
        "echo collected-stdout\nsleep 30 >&2 &\nchild=$!\necho held-stderr >&2\nprintf '%s %s\\n' \"$$\" \"$child\" > \"$PID_FILE\"\nexit 0",
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
    wait_for_shell_exit(pids[0]).await;
    token.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("new message must remain active until output collection finishes")
        .unwrap()
        .unwrap();
    let text = result_text(&result);
    assert_eq!(extract_field(text, "status"), "interrupted", "{text}");
    assert_eq!(extract_field(text, "stopped_by"), "new_message", "{text}");
    assert!(text.contains("\ncollected-stdout\n"), "{text}");
    assert!(text.contains("\nheld-stderr\n"), "{text}");
    assert_reaped(&pids[..1]);
    // An orphan is reaped by init, not our call. A zombie no longer holds pipes
    // or executes code; the owned group leader must already be fully reaped.
    assert!(process_state(pids[1])
        .await
        .is_none_or(|state| state.starts_with('Z')));
}
