use super::*;
use std::sync::atomic::Ordering;

const DEADLINE: Duration = Duration::from_secs(10);

#[tokio::test]
async fn event_hook_tool_timeout_cancels_provider_but_not_turn_or_next_entry() {
    for (python, source) in [
        (
            false,
            "add_context before; hook_probe --hang; add_context after",
        ),
        (
            true,
            "add_context('before')\ncall_tool('hook_probe', {'hang': True})\nadd_context('after')",
        ),
    ] {
        let mut entry = tool_hook(python, source, EventHookOnError::Allow);
        if let EventHookEntry::Full(definition) = &mut entry {
            definition.timeout = Duration::from_secs(1);
        }
        let mut next = tool_hook(
            false,
            "add_context \"$(hook_probe --message-value next -o text)\"",
            EventHookOnError::Deny,
        );
        if let EventHookEntry::Full(definition) = &mut next {
            definition.name = "next".into();
        }
        let fixture = Fixture::new(vec![entry, next]).await;
        let abort = create_abort_signal();
        let run = fixture.run(&abort);
        tokio::pin!(run);
        tokio::select! {
            () = fixture.probe.started.notified() => {},
            result = &mut run => panic!("hook ended before provider started: {result:?}"),
            () = tokio::time::sleep(DEADLINE) => panic!("provider did not start"),
        }
        let outcome = tokio::time::timeout(DEADLINE, run).await.unwrap().unwrap();
        assert_eq!(
            fixture.probe.cancelled.load(Ordering::SeqCst),
            1,
            "python={python}, calls={:?}, outcome={outcome:?}",
            fixture.probe.calls.lock()
        );
        assert_eq!(outcome.context, ["next"]);
        assert!(
            !abort.aborted(),
            "entry timeout must not cancel shared turn"
        );
        assert_eq!(fixture.probe.calls.lock().len(), 2);
    }
}

#[tokio::test]
async fn event_hook_turn_abort_cancels_in_flight_provider() {
    for (python, source) in [
        (false, "hook_probe --hang"),
        (true, "call_tool('hook_probe', {'hang': True})"),
    ] {
        let fixture = Fixture::new(vec![tool_hook(python, source, EventHookOnError::Deny)]).await;
        let abort = create_abort_signal();
        let run = fixture.run(&abort);
        tokio::pin!(run);
        tokio::select! {
            () = fixture.probe.started.notified() => {},
            result = &mut run => panic!("hook ended before provider started: {result:?}"),
            () = tokio::time::sleep(DEADLINE) => panic!("provider did not start"),
        }
        abort.set_ctrlc();
        assert!(tokio::time::timeout(DEADLINE, run).await.unwrap().is_err());
        assert_eq!(fixture.probe.cancelled.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.probe.calls.lock().len(), 1);
    }
}

#[tokio::test]
async fn event_hook_turn_abort_cancels_pending_tool_context_with_allow_policy() {
    let outcome = abort_during_tool_context(vec![
        tool_hook(false, "add_context must-not-run", EventHookOnError::Allow),
        hook(
            "next",
            "add_context must-not-run; rewrite_prompt must-not-run; block must-not-run",
            EventHookOnError::Allow,
        ),
    ])
    .await
    .unwrap();
    assert!(outcome.context.is_empty());
    assert!(outcome.block.is_none());
    assert!(outcome.rewrite.is_none());
    assert!(outcome.resume.is_none());
}

#[tokio::test]
async fn event_hook_turn_abort_cancels_pending_tool_context_with_deny_policy() {
    for (entries, expected_name) in [
        (
            vec![tool_hook(
                false,
                "add_context must-not-run",
                EventHookOnError::Deny,
            )],
            "helper",
        ),
        (
            vec![
                tool_hook(false, "add_context must-not-run", EventHookOnError::Allow),
                hook("next", "add_context must-not-run", EventHookOnError::Deny),
            ],
            "next",
        ),
    ] {
        let error = abort_during_tool_context(entries).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("hook '{expected_name}': hook '{expected_name}' interrupted")
        );
    }
}

async fn abort_during_tool_context(
    entries: Vec<EventHookEntry>,
) -> anyhow::Result<harnx_runtime::event_hooks::EventHookOutcome> {
    let config = config_with_hooks(HookEventKind::UserPromptSubmit, entries);
    let abort = create_abort_signal();
    let input = EventHookInput {
        event: HookEventKind::UserPromptSubmit,
        ..Default::default()
    };
    let started = tokio::sync::Notify::new();
    let dropped = harnx_script::CancellationToken::new();
    let run = run_event_hooks(&config, &input, &abort, || async {
        let _drop_guard = dropped.clone().drop_guard();
        started.notify_one();
        std::future::pending().await
    });
    tokio::pin!(run);
    tokio::select! {
        () = started.notified() => {},
        result = &mut run => panic!("hooks ended before context construction started: {result:?}"),
        () = tokio::time::sleep(DEADLINE) => panic!("context construction did not start"),
    }
    abort.set_ctrlc();
    let result = tokio::time::timeout(DEADLINE, run)
        .await
        .expect("abort must not wait for tool-context construction");
    assert!(
        dropped.is_cancelled(),
        "pending context future must be dropped"
    );
    result
}

#[tokio::test]
async fn event_hook_pre_aborted_turn_skips_context_and_scripts_for_both_policies() {
    for with_tools in [false, true] {
        for on_error in [EventHookOnError::Allow, EventHookOnError::Deny] {
            let entry = if with_tools {
                tool_hook(false, "add_context must-not-run", on_error)
            } else {
                hook("helper", "add_context must-not-run", on_error)
            };
            let config = config_with_hooks(
                HookEventKind::UserPromptSubmit,
                vec![
                    entry,
                    hook(
                        "next",
                        "add_context must-not-run; rewrite_prompt must-not-run; block must-not-run",
                        EventHookOnError::Allow,
                    ),
                ],
            );
            let abort = create_abort_signal();
            abort.set_ctrlc();
            let input = EventHookInput {
                event: HookEventKind::UserPromptSubmit,
                ..Default::default()
            };
            let called = std::sync::atomic::AtomicBool::new(false);
            let result = tokio::time::timeout(
                DEADLINE,
                run_event_hooks(&config, &input, &abort, || {
                    called.store(true, Ordering::SeqCst);
                    std::future::pending()
                }),
            )
            .await
            .expect("pre-aborted hooks must return promptly");
            assert!(!called.load(Ordering::SeqCst), "factory must not be called");
            match on_error {
                EventHookOnError::Allow => {
                    let outcome = result.unwrap();
                    assert!(outcome.context.is_empty());
                    assert!(outcome.block.is_none());
                    assert!(outcome.rewrite.is_none());
                    assert!(outcome.resume.is_none());
                }
                EventHookOnError::Deny => assert_eq!(
                    result.unwrap_err().to_string(),
                    "hook 'helper': hook 'helper' interrupted"
                ),
            }
        }
    }
}
