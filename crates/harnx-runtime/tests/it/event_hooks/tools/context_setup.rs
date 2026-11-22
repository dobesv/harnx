use super::*;
use harnx_runtime::event_hooks::EventHookOutcome;
use std::sync::atomic::{AtomicUsize, Ordering};

const SETUP_TIMEOUT: Duration = Duration::from_millis(60);

#[tokio::test]
async fn event_hook_tool_context_timeout_allows_tool_free_entries_in_order() {
    let outcome = pending_tool_context(EventHookOnError::Allow).await.unwrap();
    assert_eq!(outcome.context, ["before", "between", "after"]);
    assert_eq!(outcome.rewrite.as_deref(), Some("rewritten"));
    assert!(outcome.block.is_none());
    assert!(outcome.resume.is_none());
}

#[tokio::test]
async fn event_hook_tool_context_timeout_deny_names_the_failing_entry() {
    let error = pending_tool_context(EventHookOnError::Deny)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "hook 'longer': tool context setup timed out after 60ms"
    );
}

fn setup_entries(on_error: EventHookOnError) -> Vec<EventHookEntry> {
    let mut short = tool_hook(false, "add_context must-not-run", EventHookOnError::Allow);
    let mut longer = tool_hook(true, "add_context('must-not-run')", on_error);
    if let EventHookEntry::Full(definition) = &mut short {
        definition.timeout = Duration::from_millis(20);
    }
    if let EventHookEntry::Full(definition) = &mut longer {
        definition.name = "longer".into();
        definition.timeout = SETUP_TIMEOUT;
    }
    vec![
        hook("before", "add_context before", EventHookOnError::Deny),
        short,
        hook(
            "between",
            "add_context between; rewrite_prompt rewritten",
            EventHookOnError::Deny,
        ),
        longer,
        hook("after", "add_context after", EventHookOnError::Deny),
    ]
}

async fn pending_tool_context(on_error: EventHookOnError) -> anyhow::Result<EventHookOutcome> {
    let config = config_with_hooks(HookEventKind::UserPromptSubmit, setup_entries(on_error));
    let abort = create_abort_signal();
    let input = EventHookInput {
        event: HookEventKind::UserPromptSubmit,
        ..Default::default()
    };
    let builds = AtomicUsize::new(0);
    let dropped = harnx_script::CancellationToken::new();
    let started = tokio::time::Instant::now();
    let run = run_event_hooks(&config, &input, &abort, || async {
        builds.fetch_add(1, Ordering::SeqCst);
        let _drop_guard = dropped.clone().drop_guard();
        std::future::pending().await
    });
    let result = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("tool context setup must have a time bound");
    assert!(
        started.elapsed() >= SETUP_TIMEOUT,
        "shared setup must use the longest tool entry timeout"
    );
    assert_eq!(builds.load(Ordering::SeqCst), 1, "no discovery retries");
    assert!(
        dropped.is_cancelled(),
        "pending setup future must be dropped"
    );
    assert!(!abort.aborted(), "setup timeout must not abort the turn");
    result
}
