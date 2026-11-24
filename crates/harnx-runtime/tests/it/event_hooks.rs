//! In-process lifecycle event-hook behavior (plan D7): composition rules,
//! `on_error` policies, Stop resume ordering, StopFailure diagnostics, and
//! legacy-config rejection with migration guidance.

mod tools;

use harnx_core::hooks::{
    EventHookDefinition, EventHookEntry, EventHookOnError, EventHooksList, HookEventKind,
    HooksConfig,
};
use harnx_core::middleware::MiddlewareSourceConfig;
use harnx_runtime::config::{Config, ConfigLock, GlobalConfig};
use harnx_runtime::event_hooks::{run_event_hooks, EventHookInput};
use harnx_runtime::utils::create_abort_signal;
use std::sync::Arc;
use std::time::Duration;

fn hook(name: &str, bash: &str, on_error: EventHookOnError) -> EventHookEntry {
    EventHookEntry::Full(EventHookDefinition {
        name: name.to_string(),
        source: MiddlewareSourceConfig::Bash {
            bash: bash.to_string(),
        },
        tools: vec![],
        fetch: vec![],
        timeout: Duration::from_secs(30),
        on_error,
    })
}

fn python_hook(name: &str, python: &str) -> EventHookEntry {
    EventHookEntry::Full(EventHookDefinition {
        name: name.to_string(),
        source: MiddlewareSourceConfig::Python {
            python: python.to_string(),
        },
        tools: vec![],
        fetch: vec![],
        timeout: Duration::from_secs(30),
        on_error: EventHookOnError::Allow,
    })
}

// qual:allow(dry, fragment) reason: "Mirrors the unit-tree builder; unit and integration test binaries cannot share helpers without exporting test-only code."
fn config_with_hooks(event: HookEventKind, entries: Vec<EventHookEntry>) -> GlobalConfig {
    let list = EventHooksList { entries };
    let mut hooks = HooksConfig::default();
    match event {
        HookEventKind::SessionStart => hooks.session_start = list,
        HookEventKind::UserPromptSubmit => hooks.user_prompt_submit = list,
        HookEventKind::Stop => hooks.stop = list,
        HookEventKind::StopFailure => hooks.stop_failure = list,
    }
    let config = Config {
        data: harnx_core::config_data::ConfigData {
            hooks: Some(Box::new(hooks)),
            ..Default::default()
        },
        ..Config::default()
    };
    Arc::new(ConfigLock::new(config))
}

async fn run(
    config: &GlobalConfig,
    event: HookEventKind,
    prompt: Option<&str>,
) -> anyhow::Result<harnx_runtime::event_hooks::EventHookOutcome> {
    run_event_hooks(
        config,
        &EventHookInput {
            event,
            session_id: "test-session".to_string(),
            cwd: std::env::temp_dir(),
            resume_count: 0,
            prompt: prompt.map(str::to_string),
            ..Default::default()
        },
        &create_abort_signal(),
        || async { panic!("hooks without tools must not build a tool context") },
    )
    .await
}

#[tokio::test]
async fn event_hooks_session_start_context_accumulates_in_order() {
    let config = config_with_hooks(
        HookEventKind::SessionStart,
        vec![
            hook("first", "add_context \"one\"", EventHookOnError::Allow),
            hook("second", "add_context \"two\"", EventHookOnError::Allow),
        ],
    );
    let outcome = run(&config, HookEventKind::SessionStart, None)
        .await
        .unwrap();
    assert_eq!(outcome.context, vec!["one", "two"]);
    assert!(outcome.block.is_none());
    assert!(outcome.rewrite.is_none());
}

#[tokio::test]
async fn event_hooks_python_session_start_hook_adds_context() {
    let config = config_with_hooks(
        HookEventKind::SessionStart,
        vec![python_hook("greeter", "add_context(\"from python\")")],
    );
    let outcome = run(&config, HookEventKind::SessionStart, None)
        .await
        .unwrap();
    assert_eq!(outcome.context, vec!["from python"]);
}

#[test]
fn session_start_hook_payload_carries_source_model_and_session() {
    let payload = EventHookInput {
        event: HookEventKind::SessionStart,
        session_id: "fresh".to_string(),
        cwd: std::env::temp_dir(),
        resume_count: 0,
        source: Some("startup".to_string()),
        model: Some("test:test-model".to_string()),
        ..Default::default()
    }
    .to_json();
    assert_eq!(payload["source"], "startup");
    assert_eq!(payload["model"], "test:test-model");
    assert_eq!(payload["session_id"], "fresh");
    assert_eq!(payload["event"], "SessionStart");
}

#[tokio::test]
async fn event_hooks_user_prompt_submit_composition_rewrite_and_context() {
    let config = config_with_hooks(
        HookEventKind::UserPromptSubmit,
        vec![
            hook(
                "rephrase",
                "rewrite_prompt \"rewritten prompt\"\nadd_context \"first\"",
                EventHookOnError::Allow,
            ),
            // The second hook sees the rewritten prompt through event_input.
            python_hook(
                "witness",
                "payload = event_input()\nadd_context(payload[\"prompt\"])",
            ),
        ],
    );
    let outcome = run(&config, HookEventKind::UserPromptSubmit, Some("raw prompt"))
        .await
        .unwrap();
    assert_eq!(outcome.rewrite.as_deref(), Some("rewritten prompt"));
    assert_eq!(outcome.context, vec!["first", "rewritten prompt"]);
}

#[tokio::test]
async fn event_hooks_user_prompt_submit_block_discards_context_and_skips_later_hooks() {
    let config = config_with_hooks(
        HookEventKind::UserPromptSubmit,
        vec![
            hook(
                "gate",
                "add_context \"doomed\"\nblock \"not allowed\"",
                EventHookOnError::Allow,
            ),
            hook(
                "witness",
                "add_context \"must not run\"",
                EventHookOnError::Allow,
            ),
        ],
    );
    let outcome = run(&config, HookEventKind::UserPromptSubmit, Some("raw prompt"))
        .await
        .unwrap();
    assert_eq!(outcome.block.as_deref(), Some("not allowed"));
    assert!(
        outcome.context.is_empty(),
        "a block discards the whole chain's context"
    );
}

#[tokio::test]
async fn event_hooks_on_error_allow_drops_failed_run_effects_and_continues() {
    let config = config_with_hooks(
        HookEventKind::UserPromptSubmit,
        vec![
            hook(
                "broken",
                "add_context \"dropped\"\nexit 1",
                EventHookOnError::Allow,
            ),
            hook("keeper", "add_context \"kept\"", EventHookOnError::Allow),
        ],
    );
    let outcome = run(&config, HookEventKind::UserPromptSubmit, Some("prompt"))
        .await
        .unwrap();
    assert_eq!(
        outcome.context,
        vec!["kept"],
        "the failed run's effects are dropped and later hooks still run"
    );
}

#[tokio::test]
async fn event_hooks_on_error_deny_blocks_naming_the_hook() {
    let config = config_with_hooks(
        HookEventKind::UserPromptSubmit,
        vec![hook("gatekeeper", "exit 1", EventHookOnError::Deny)],
    );
    let error = run(&config, HookEventKind::UserPromptSubmit, Some("prompt"))
        .await
        .expect_err("deny must fail the event");
    assert!(
        error.to_string().contains("gatekeeper"),
        "the error must name the hook: {error:#}"
    );
}

/// Interaction operations are intentionally not registered for event hooks:
/// using one fails the run, which the on_error policy handles.
#[tokio::test]
async fn event_hooks_interactions_are_unavailable() {
    let config = config_with_hooks(
        HookEventKind::UserPromptSubmit,
        vec![
            hook("chatty", "confirm \"ok?\"", EventHookOnError::Allow),
            hook("keeper", "add_context \"kept\"", EventHookOnError::Allow),
        ],
    );
    let outcome = run(&config, HookEventKind::UserPromptSubmit, Some("prompt"))
        .await
        .unwrap();
    assert_eq!(outcome.context, vec!["kept"]);
}

#[tokio::test]
async fn event_hooks_stop_first_resume_wins_and_skips_later_hooks() {
    let config = config_with_hooks(
        HookEventKind::Stop,
        vec![
            hook(
                "resume-first",
                "resume \"first message\"",
                EventHookOnError::Allow,
            ),
            // If the chain wrongly continued past the first resume, this hook
            // would run and fail under deny, failing the whole event.
            hook("resume-second", "resume \"second\"", EventHookOnError::Deny),
        ],
    );
    let outcome = run(&config, HookEventKind::Stop, None).await.unwrap();
    assert_eq!(outcome.resume.as_deref(), Some("first message"));
}

#[test]
fn event_hooks_stop_failure_event_input_carries_diagnostics() {
    let payload = EventHookInput {
        event: HookEventKind::StopFailure,
        session_id: "failing".to_string(),
        cwd: std::env::temp_dir(),
        resume_count: 2,
        error: Some("provider request failed: 429".to_string()),
        error_type: Some("api_error".to_string()),
        ..Default::default()
    }
    .to_json();
    assert_eq!(payload["error"], "provider request failed: 429");
    assert_eq!(payload["error_type"], "api_error");
    assert_eq!(payload["event"], "StopFailure");
    assert_eq!(payload["resume_count"], 2);
}

/// StopFailure is observe-only: `add_context` is not one of its operations, so
/// the run fails and, under `allow`, the event completes with no effects.
#[tokio::test]
async fn event_hooks_stop_failure_rejects_effect_operations() {
    let config = config_with_hooks(
        HookEventKind::StopFailure,
        vec![hook(
            "observer",
            "add_context \"must not work\"",
            EventHookOnError::Allow,
        )],
    );
    let outcome = run(&config, HookEventKind::StopFailure, None)
        .await
        .unwrap();
    assert!(outcome.context.is_empty());
}

#[test]
fn event_hooks_legacy_command_entries_fail_with_migration_guidance() {
    let legacy = "entries:\n  - command: retired-hook-server --event Stop -- /hook.sh\n";
    let error = serde_yaml::from_str::<HooksConfig>(legacy)
        .unwrap_err()
        .to_string();
    assert!(error.contains("legacy"), "{error}");
    assert!(error.contains("middleware"), "{error}");
    assert!(error.contains("in-process"), "{error}");

    let legacy_event = "Stop:\n  - command: retired-hook-server --event Stop -- /hook.sh\n";
    let error = serde_yaml::from_str::<HooksConfig>(legacy_event)
        .unwrap_err()
        .to_string();
    assert!(error.contains("legacy `command:`"), "{error}");
}

/// The full config-load path rejects legacy entries too, so nothing that ships
/// with a legacy `hooks:` section can start.
#[test]
fn event_hooks_legacy_entries_fail_config_load() {
    let temp = tempfile::tempdir().unwrap();
    let config_path = temp.path().join("config.yaml");
    std::fs::write(
        &config_path,
        "hooks:\n  entries:\n    - command: retired-hook-server --event Stop -- /hook.sh\n",
    )
    .unwrap();
    let error =
        Config::load_from_file(&config_path).expect_err("legacy hooks must fail config load");
    assert!(format!("{error:#}").contains("legacy"), "{error:#}");
}
