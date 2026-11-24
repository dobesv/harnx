//! Hook → shared script bridge → middleware → instrumented provider.
mod cancellation;
mod context_setup;
mod fixture;

use super::*;
use fixture::{tool_hook, Fixture};
use serde_json::json;

#[tokio::test]
async fn event_hook_bash_tool_output_becomes_context() {
    let fixture = Fixture::new(vec![tool_hook(false, r#"
add_context "$(hook_probe --message-value 'from bash' -o text)"
add_context "$(hook_probe --json '{"message_value":"from json"}' -o json | jq -r '.content[0].text')"
"#, EventHookOnError::Deny)]).await;
    let outcome = fixture.run(&create_abort_signal()).await.unwrap();
    assert_eq!(outcome.context, ["from bash", "from json"]);
    let calls = fixture.probe.calls.lock();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].name, "hook_probe");
    assert_eq!(calls[0].arguments["message_value"], "from bash");
    assert_eq!(calls[1].arguments["message_value"], "from json");
    assert!(calls.iter().all(|call| call.round.is_none()));
    assert_ne!(calls[0].id, calls[1].id);
    for call in calls.iter() {
        uuid::Uuid::parse_str(&call.id).expect("fresh UUID");
    }
}

#[tokio::test]
async fn event_hook_python_tool_output_becomes_context() {
    let fixture = Fixture::new(vec![tool_hook(
        true,
        r#"
r = call_tool("hook_probe", {"message_value": "from python"})
add_context(r["content"][0]["text"])
r = call_tool(args={"message_value": "from kwargs"}, name="hook_probe")
add_context(r["content"][0]["text"])
"#,
        EventHookOnError::Deny,
    )])
    .await;
    let outcome = fixture.run(&create_abort_signal()).await.unwrap();
    assert_eq!(outcome.context, ["from python", "from kwargs"]);
    let calls = fixture.probe.calls.lock();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|call| call.round.is_none()));
    assert_ne!(calls[0].id, calls[1].id);
}

#[tokio::test]
async fn event_hook_non_allowlisted_tool_never_dispatches_and_on_error_applies() {
    for (python, source) in [
        (
            false,
            "add_context before; hook_forbidden --json '{}'; add_context after",
        ),
        (
            true,
            "add_context('before')\ncall_tool('hook_forbidden', {})\nadd_context('after')",
        ),
    ] {
        for on_error in [EventHookOnError::Allow, EventHookOnError::Deny] {
            let fixture = Fixture::new(vec![
                tool_hook(python, source, on_error),
                hook("next", "add_context next", EventHookOnError::Allow),
            ])
            .await;
            let result = fixture.run(&create_abort_signal()).await;
            assert!(fixture.probe.calls.lock().is_empty());
            match on_error {
                EventHookOnError::Allow => assert_eq!(result.unwrap().context, ["next"]),
                EventHookOnError::Deny => {
                    let message = result.unwrap_err().to_string();
                    assert!(message.contains("helper"), "{message}");
                    if !python {
                        assert!(message.contains("hook_forbidden"), "{message}");
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn event_hook_recoverable_tool_errors_keep_later_effects() {
    assert_recoverable_tool_results([
        (false, r#"r=$(hook_probe --recoverable); add_context "$(echo "$r" | jq -r '.is_error')"; add_context after"#),
        (true, "r = call_tool('hook_probe', {'recoverable': True})\nassert r['is_error']\nassert 'hook provider failure' in r['error']\nadd_context('true')\nadd_context('after')"),
    ]).await;
}

#[tokio::test]
async fn event_hook_middleware_block_is_recoverable_and_never_dispatches() {
    for (python, source) in [
        (false, r#"r=$(hook_probe --json '{}'); add_context "$(echo "$r" | jq -r '.is_error')"; add_context "$(echo "$r" | jq -r '.blocked_by_middleware')"; add_context after"#),
        (true, "r = call_tool('hook_probe', {})\nassert r['is_error']\nadd_context('true')\nadd_context(r['blocked_by_middleware'])\nadd_context('after')"),
    ] {
        let fixture = Fixture::with_middleware(
            vec![tool_hook(python, source, EventHookOnError::Deny)],
            vec![json!({"name":"hook-policy", "match":"^hook_probe$", "bash":"deny 'hook calls forbidden'"})],
        ).await;
        let outcome = fixture.run(&create_abort_signal()).await.unwrap();
        assert_eq!(outcome.context, ["true", "hook-policy", "after"]);
        assert!(fixture.probe.calls.lock().is_empty());
    }
}

#[tokio::test]
async fn event_hook_configured_middleware_wraps_tool_calls() {
    let fixture = Fixture::with_middleware(
        vec![tool_hook(true,
            "r = call_tool('hook_probe', {'message_value':'original'})\nadd_context(r['content'][0]['text'])",
            EventHookOnError::Deny)],
        vec![json!({"name":"hook-policy", "match":"^hook_probe$", "python":r#"
a = tool_input()["arguments"]
a["message_value"] += "-policy"
r = call_next(a)
r["output"]["content"][0]["text"] += "-result"
respond(r)
"#})],
    ).await;
    let outcome = fixture.run(&create_abort_signal()).await.unwrap();
    assert_eq!(outcome.context, ["original-policy-result"]);
    assert_eq!(
        fixture.probe.calls.lock()[0].arguments["message_value"],
        "original-policy"
    );
}

#[tokio::test]
async fn event_hook_fatal_tool_errors_fail_entry_and_drop_effects() {
    for (python, source) in [
        (
            false,
            "add_context before; hook_probe --fatal; add_context after",
        ),
        (
            true,
            "add_context('before')\ncall_tool('hook_probe', {'fatal': True})\nadd_context('after')",
        ),
    ] {
        let fixture = Fixture::new(vec![
            tool_hook(python, source, EventHookOnError::Allow),
            hook("next", "add_context next", EventHookOnError::Allow),
        ])
        .await;
        assert_eq!(
            fixture.run(&create_abort_signal()).await.unwrap().context,
            ["next"]
        );
        assert_eq!(fixture.probe.calls.lock().len(), 1);
    }
}

#[tokio::test]
async fn event_hook_glob_tool_collisions_still_fail_without_replacing_operations() {
    for python in [false, true] {
        let mut entry = tool_hook(python, "add_context('bad')", EventHookOnError::Deny);
        if let EventHookEntry::Full(definition) = &mut entry {
            definition.tools = vec!["add_*".into()];
        }
        let fixture = Fixture::new(vec![entry]).await;
        let error = fixture
            .run(&create_abort_signal())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("allowlisted tool collides with an event-hook operation"),
            "{error}"
        );
        assert!(fixture.probe.calls.lock().is_empty());
    }
}

#[tokio::test]
async fn event_hook_context_is_lazy_once_per_event_and_tools_are_per_entry() {
    let mut second = tool_hook(true, "call_tool('hook_probe', {})", EventHookOnError::Deny);
    if let EventHookEntry::Full(definition) = &mut second {
        definition.name = "second".into();
    }
    let fixture = Fixture::new(vec![
        tool_hook(false, "hook_probe --json '{}'", EventHookOnError::Deny),
        second,
        hook(
            "unprivileged",
            "hook_probe --json '{}'",
            EventHookOnError::Allow,
        ),
        hook("after", "add_context after", EventHookOnError::Allow),
    ])
    .await;
    let builds = std::sync::atomic::AtomicUsize::new(0);
    let outcome = run_event_hooks(
        &fixture.config,
        &EventHookInput::default(),
        &create_abort_signal(),
        || async {
            builds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            fixture.context.clone()
        },
    )
    .await
    .unwrap();
    assert_eq!(builds.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(outcome.context, ["after"]);
    assert_eq!(fixture.probe.calls.lock().len(), 2);
}

#[tokio::test]
async fn event_hook_without_tools_does_not_build_context_or_expose_call_tool() {
    for entry in [
        hook(
            "unprivileged",
            "hook_probe --json '{}'",
            EventHookOnError::Allow,
        ),
        python_hook("unprivileged", "call_tool('hook_probe', {})"),
    ] {
        let config = config_with_hooks(
            HookEventKind::SessionStart,
            vec![
                entry,
                hook("after", "add_context after", EventHookOnError::Allow),
            ],
        );
        assert_eq!(
            run(&config, HookEventKind::SessionStart, None)
                .await
                .unwrap()
                .context,
            ["after"]
        );
    }
}

#[tokio::test]
async fn event_hook_provider_interaction_is_recoverable_without_suspending() {
    assert_recoverable_tool_results([
        (false, r#"r=$(hook_probe --interact); add_context "$(echo "$r" | jq -r '.is_error')"; add_context after"#),
        (true, "r = call_tool('hook_probe', {'interact': True})\nassert r['is_error']\nadd_context('true')\nadd_context('after')"),
    ]).await;
}

#[tokio::test]
async fn event_hook_middleware_interaction_is_recoverable_without_suspending() {
    let fixture = Fixture::with_middleware(
        vec![tool_hook(true,
            "r = call_tool('hook_probe', {})\nassert r['is_error']\nadd_context('after')",
            EventHookOnError::Deny)],
        vec![json!({"name":"hook-policy", "match":"^hook_probe$", "python":"confirm('Approve hook?')\ncall_next()"})],
    ).await;
    assert_eq!(
        fixture.run(&create_abort_signal()).await.unwrap().context,
        ["after"]
    );
    assert!(fixture.probe.calls.lock().is_empty());
}

#[tokio::test]
async fn event_hook_python_cannot_catch_host_allowlist_failure() {
    let fixture = Fixture::new(vec![
        tool_hook(true, "add_context('before')\ntry:\n    call_tool('hook_forbidden', {})\nexcept Exception:\n    add_context('caught')", EventHookOnError::Allow),
        hook("next", "add_context next", EventHookOnError::Allow),
    ]).await;
    assert_eq!(
        fixture.run(&create_abort_signal()).await.unwrap().context,
        ["next"]
    );
    assert!(fixture.probe.calls.lock().is_empty());
}

async fn assert_recoverable_tool_results(scripts: [(bool, &str); 2]) {
    for (python, source) in scripts {
        let fixture = Fixture::new(vec![tool_hook(python, source, EventHookOnError::Deny)]).await;
        let outcome = fixture.run(&create_abort_signal()).await.unwrap();
        assert_eq!(outcome.context, ["true", "after"]);
        assert_eq!(fixture.probe.calls.lock().len(), 1);
    }
}
