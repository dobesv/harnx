use super::*;

const BASH_SAVE: &str = r#"
if saved result > /tmp/r; then :; else
  next --id run > /tmp/r
  save result "$(cat /tmp/r)"
fi
if confirm --id keep 'Keep this result?'; then
  respond "$(cat /tmp/r)"
else
  deny 'Result discarded. The command already ran.'
fi
"#;
const PYTHON_SAVE: &str = r#"
result = saved('result')
if result is None:
    result = call_next(id='run')
    save('result', result)
if confirm('Keep this result?', id='keep'):
    respond(result)
else:
    deny('Result discarded. The command already ran.')
"#;

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_saved_envelope_retains_media_and_private_observation_both_languages() {
    for entry in [bash(BASH_SAVE), python(PYTHON_SAVE)] {
        for action in ["approve", "deny"] {
            let mut harness = Harness::new(vec![entry.clone()]).await;
            let args = json!({"value":"second", "media":true});
            let request = pending(harness.attempt(args.clone()).await);
            assert_eq!(harness.fixture.calls().len(), 1);
            assert_eq!(request.checkpoint.snapshots.len(), 1);
            let stash = &request.checkpoint.state["frames"][0]["stash"]["result"];
            assert!(stash["ref"].is_string());
            assert!(stash.get("content").is_none());
            assert!(stash.get("execution_context").is_none());
            assert!(!request
                .checkpoint
                .state
                .to_string()
                .contains("workspace_root"));
            assert!(!request
                .interaction
                .payload
                .to_string()
                .contains("workspace_root"));
            harness.accept(&request, action);
            let result = completed(harness.attempt(args).await);
            assert_eq!(harness.fixture.calls().len(), 1);
            if action == "approve" {
                assert_eq!(result.images().count(), 1);
                assert_eq!(
                    serde_json::to_value(&result.content).unwrap()[0]["image_url"]["url"],
                    "data:image/png;base64,c2Vjb25k"
                );
                assert!(result
                    .execution_context
                    .unwrap()
                    .workspace_root
                    .ends_with("second"));
            } else {
                assert_eq!(
                    result.output["error"],
                    "Result discarded. The command already ran."
                );
                assert!(result.content.is_empty());
                assert!(result.execution_context.is_none());
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_nested_checkpoint_includes_ancestors_and_completed_frames() {
    let mut outer = bash(
        r#"
if saved pre > /tmp/r; then :; else
  next --id pre '{"stage":"pre","value":"second","media":true}' > /tmp/r
  save pre "$(cat /tmp/r)"
fi
save marker '"outer"'
next --id gated '{"stage":"gated"}' > /tmp/g
respond "$(cat /tmp/r)"
"#,
    );
    outer["name"] = json!("outer");
    let mut inner = python(
        r#"
if tool_input()['arguments']['stage'] == 'pre':
    result = call_next()
    save('result', result)
    respond(result)
else:
    save('marker', 'inner')
    confirm('Nested?', id='gate')
    call_next()
"#,
    );
    inner["name"] = json!("inner");
    let mut harness = Harness::new(vec![outer, inner]).await;
    let request = pending(harness.attempt(json!({})).await);
    let frames = request.checkpoint.state["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 3);
    assert!(frames
        .iter()
        .any(|frame| frame["middleware"] == "outer" && frame["stash"]["marker"] == "outer"));
    assert!(frames
        .iter()
        .any(|frame| frame["middleware"] == "inner" && frame["stash"]["marker"] == "inner"));
    assert!(
        frames
            .iter()
            .any(|frame| frame["middleware"] == "inner"
                && frame["stash"]["result"]["ref"].is_string())
    );
    assert_eq!(request.checkpoint.snapshots.len(), 2);
    assert_eq!(request.interaction.step_path[0].key, "gated");
    harness.accept(&request, "approve");
    let result = completed(harness.attempt(json!({})).await);
    assert_eq!(result.images().count(), 1);
    assert!(result.execution_context.is_some());
    let calls = harness.fixture.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].1["stage"], "pre");
    assert_eq!(calls[1].1["stage"], "gated");
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_missing_retained_snapshot_fails_before_dispatch() {
    let mut harness = Harness::new(vec![python(PYTHON_SAVE)]).await;
    let request = pending(
        harness
            .attempt(json!({"value":"second","media":true}))
            .await,
    );
    harness.accept(&request, "approve");
    harness.store.checkpoint.as_mut().unwrap().snapshots.clear();
    let result = completed(harness.attempt(json!({})).await);
    assert!(result
        .output
        .to_string()
        .contains("missing retained middleware snapshot"));
    assert_eq!(harness.fixture.calls().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_old_ref_not_valid_until_saved_restores_envelope() {
    let source = format!("if 'raw' in tool_input()['arguments']:\n    respond(tool_input()['arguments']['raw'])\nelse:\n{}", PYTHON_SAVE.trim().lines().map(|line| format!("    {line}")).collect::<Vec<_>>().join("\n"));
    let mut harness = Harness::new(vec![python(&source)]).await;
    let request = pending(
        harness
            .attempt(json!({"value":"second","media":true}))
            .await,
    );
    harness.accept(&request, "approve");
    let envelope = request.checkpoint.state["frames"][0]["stash"]["result"].clone();
    let result = completed(harness.attempt(json!({"raw":envelope})).await);
    assert!(result
        .output
        .to_string()
        .contains("unknown or wrong-scope middleware result ref"));
    assert!(result.content.is_empty());
    assert_eq!(harness.fixture.calls().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_stash_limit_fails_without_dispatch_both_languages() {
    for entry in [
        bash("save oversized \"$(python3 -c 'import json; print(json.dumps(\"x\" * 262145))')\""),
        python("save('oversized', 'x' * 262145)"),
    ] {
        let mut harness = Harness::new(vec![entry]).await;
        let result = completed(harness.attempt(json!({})).await);
        assert!(
            result
                .output
                .to_string()
                .contains("middleware stash exceeds 256 KiB"),
            "{:?}",
            result.output
        );
        assert!(harness.fixture.calls().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_nested_saved_envelopes_restore_their_refs() {
    let mut harness = Harness::new(vec![python(
        "payload = saved('payload')\nif payload is None:\n    payload = {'results': [call_next(id='run')]}\n    save('payload', payload)\nconfirm('Keep?', id='keep')\nrespond(payload['results'][0])"
    )]).await;
    let args = json!({"value":"second","media":true});
    let request = pending(harness.attempt(args.clone()).await);
    assert_eq!(request.checkpoint.snapshots.len(), 1);
    harness.accept(&request, "approve");
    let result = completed(harness.attempt(args).await);
    assert_eq!(result.images().count(), 1);
    assert!(result.execution_context.is_some());
    assert_eq!(harness.fixture.calls().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_overwriting_stash_releases_retained_snapshot() {
    let mut harness = Harness::new(vec![python(
        "save('result', call_next())\nsave('result', {'discarded': True})\nconfirm('Stop?')",
    )])
    .await;
    let request = pending(
        harness
            .attempt(json!({"value":"second","media":true}))
            .await,
    );
    assert!(request.checkpoint.snapshots.is_empty());
    assert_eq!(
        request.checkpoint.state["frames"][0]["stash"]["result"],
        json!({"discarded":true})
    );
    assert_eq!(harness.fixture.calls().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_confirm_checkpoint_limit_counts_retained_results() {
    let mut harness = Harness::new(vec![python(PYTHON_SAVE)]).await;
    let request = pending(harness.attempt(json!({})).await);
    harness.accept(&request, "approve");
    let checkpoint = harness.store.checkpoint.as_mut().unwrap();
    checkpoint
        .snapshots
        .values_mut()
        .next()
        .unwrap()
        .result
        .output = json!("x".repeat(8 * 1024 * 1024));
    let result = completed(harness.attempt(json!({})).await);
    assert!(result
        .output
        .to_string()
        .contains("middleware checkpoint exceeds 8 MiB"));
    assert_eq!(harness.fixture.calls().len(), 1);
}
