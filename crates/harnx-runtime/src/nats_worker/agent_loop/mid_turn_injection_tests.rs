//! NATS-backed tests for `build_mid_turn_injection_callback`.
//!
//! These reproduce issue dobesv/harnx#2493: durable user rows that lose their
//! admission identity (or keep an older invocation's identity through
//! compaction copies) used to fail mid-turn injection with "injected user row
//! has no admission identity" or a competing-execution error.

use super::build_mid_turn_injection_callback;
use crate::nats_session_metadata::{
    AdmissionAuthority, InvocationAdmission, SessionInitializer, SessionMetadata,
    SessionMetadataStore,
};
use harnx_core::input::Input;
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::require_nextest;
use harnx_core::session::SessionLogEntry;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

struct UserRow<'a> {
    id: Option<&'a str>,
    text: &'a str,
}

impl UserRow<'_> {
    fn into_entry(self) -> SessionLogEntry {
        SessionLogEntry::Message {
            id: self.id.map(str::to_owned),
            role: MessageRole::User,
            content: MessageContent::Text(self.text.to_string()),
            timestamp: None,
            fence_token: None,
        }
    }
}

fn assistant_row(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::Assistant,
        content: MessageContent::Text(text.to_string()),
        timestamp: None,
        fence_token: None,
    }
}

fn turn_end_row(through_seq: u64) -> SessionLogEntry {
    SessionLogEntry::TurnEnd {
        through_seq,
        fence_token: 0,
        timestamp: None,
        usage: None,
    }
}

fn compress_row(summary: &str) -> SessionLogEntry {
    SessionLogEntry::Compress {
        prompt: summary.to_string(),
    }
}

struct InjectionHarness {
    child: crate::nats_worker::tests::TestNatsServer,
    _store_dir: tempfile::TempDir,
    log: crate::nats_session_log::NatsSessionLog,
    store: SessionMetadataStore,
    storage_key: String,
    backend: crate::nats_worker::NatsSessionLogBackend,
    invocation_id: String,
}

/// Admit `prompt_id` under a fresh invocation and bind the prompt admission.
/// Returns the invocation id.
async fn admit(harness: &InjectionHarness, prompt_id: &str) -> anyhow::Result<String> {
    let entries = harness.log.load_events_async().await?;
    let proposed = InvocationAdmission::new(
        &AdmissionAuthority::External {
            admitted_at: chrono::Utc::now(),
        },
        prompt_id.to_string(),
        None,
        None,
    );
    let intent = harness
        .store
        .reserve_admission(&harness.storage_key, &proposed, &entries)
        .await?;
    harness
        .store
        .bind_prompt_admission(
            &harness.storage_key,
            prompt_id,
            intent.invocation_id.as_str(),
        )
        .await?;
    Ok(intent.invocation_id.as_str().to_string())
}

async fn spawn_harness(name: &str) -> anyhow::Result<Option<InjectionHarness>> {
    let Some((url, child, store_dir)) = crate::nats_worker::tests::spawn_test_nats().await else {
        return Ok(None);
    };
    let client = async_nats::connect(&url).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let session_id = crate::nats_worker::new_remote_session_id();
    let storage_key = harnx_core::session_identity::session_key(Some(name), &session_id);
    store
        .create(&SessionMetadata::new(
            &session_id,
            SessionInitializer::named(name, Default::default()),
        ))
        .await?;
    let log = crate::nats_session_log::NatsSessionLog::new_with_replicas(
        jetstream.clone(),
        storage_key.clone(),
        1,
    );
    let backend = crate::nats_worker::NatsSessionLogBackend::new(jetstream, storage_key.clone(), 1)
        .with_metadata_store(Some(store.clone()));
    Ok(Some(InjectionHarness {
        child,
        _store_dir: store_dir,
        log,
        store,
        storage_key,
        backend,
        invocation_id: String::new(),
    }))
}

impl InjectionHarness {
    async fn append_user(&self, row: UserRow<'_>) -> anyhow::Result<u64> {
        self.append(row.into_entry()).await
    }

    async fn append(&self, entry: SessionLogEntry) -> anyhow::Result<u64> {
        self.log.append_event_async(&entry).await
    }

    /// Bind a steering prompt to the running invocation, the way interactive
    /// steering admits follow-up prompts mid-turn.
    async fn bind_steering(&self, prompt_id: &str) -> anyhow::Result<()> {
        self.store
            .bind_prompt_admission(&self.storage_key, prompt_id, &self.invocation_id)
            .await
    }

    fn callback(&self, cursor: Arc<AtomicU64>) -> crate::agent_loop::OnToolRoundFn {
        build_mid_turn_injection_callback(self.backend.clone(), cursor, self.invocation_id.clone())
    }

    fn input() -> Input {
        Input::new(
            String::new(),
            (String::new(), Vec::new()),
            harnx_core::agent_config::AgentConfig::default(),
        )
    }

    fn shutdown(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Issue #2493, path 1: an admitted prompt injected mid-turn survives a later
/// compaction re-log as a deduplicated copy, the copies are not re-injected or
/// re-verified, and a genuinely new admitted prompt still injects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_prompts_survive_mid_turn_compaction_without_reinjection() -> anyhow::Result<()> {
    require_nextest();
    let Some(mut harness) = spawn_harness("mid-turn-injection-compaction").await? else {
        eprintln!("Skipping test: nats-server not available");
        return Ok(());
    };

    // Earlier turn: an admitted prompt answered and terminated.
    let old_seq = harness
        .append_user(UserRow {
            id: Some("old-prompt"),
            text: "first task",
        })
        .await?;
    let old_invocation = admit(&harness, "old-prompt").await?;
    harness.append(assistant_row("first answer")).await?;
    harness.append(turn_end_row(old_seq)).await?;

    // Current invocation picks the session up mid-turn and a steering prompt
    // arrives during a tool round.
    let cursor = Arc::new(AtomicU64::new(old_seq));
    let steering_seq = harness
        .append_user(UserRow {
            id: Some("steering"),
            text: "keep going",
        })
        .await?;
    harness.invocation_id = admit(&harness, "steering").await?;
    assert_ne!(harness.invocation_id, old_invocation);

    let callback = harness.callback(Arc::clone(&cursor));
    let mut input = InjectionHarness::input();
    callback(&mut input, &[]).await?;
    assert_eq!(input.injected_user_text(), Some("keep going"));
    let injected = &input.injected_user_messages;
    assert_eq!(injected.len(), 1);
    assert_eq!(injected[0].id.as_deref(), Some("steering"));
    assert_eq!(injected[0].log_seq, Some(steering_seq as usize));
    assert_eq!(cursor.load(Ordering::SeqCst), steering_seq);

    // Mid-turn compaction re-logs kept messages after a fresh Compress marker:
    // a copy of the older prompt from the compacted tail and a copy of the
    // just-injected steering prompt, both carrying their original ids.
    harness
        .append(compress_row("summary of first task"))
        .await?;
    harness
        .append_user(UserRow {
            id: Some("old-prompt"),
            text: "first task",
        })
        .await?;
    harness
        .append_user(UserRow {
            id: Some("steering"),
            text: "keep going",
        })
        .await?;

    // A genuinely new admitted steering prompt unlocks the next callback. The
    // historical copy of the old invocation's prompt must not be verified or
    // injected alongside it, and the already-injected prompt must not repeat.
    harness
        .append_user(UserRow {
            id: Some("steering-2"),
            text: "and then finish",
        })
        .await?;
    harness.bind_steering("steering-2").await?;

    let mut next_input = InjectionHarness::input();
    callback(&mut next_input, &[]).await?;
    assert_eq!(next_input.injected_user_text(), Some("and then finish"));
    let next_injected = &next_input.injected_user_messages;
    assert_eq!(next_injected.len(), 1);
    assert_eq!(next_injected[0].id.as_deref(), Some("steering-2"));

    // After that injection no further rows arrive: the follow-up callback must
    // be a no-op, not an error or a re-injection.
    let mut drained_input = InjectionHarness::input();
    callback(&mut drained_input, &[]).await?;
    assert!(drained_input.injected_user_text().is_none());
    harness.shutdown();
    Ok(())
}

/// Issue #2493, path 2: compaction-re-logged runtime notes (sub-agent starts,
/// cancels) are non-executable context. They are skipped by the fold and never
/// treated as new prompts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compacted_runtime_notes_are_not_treated_as_prompts() -> anyhow::Result<()> {
    require_nextest();
    let Some(mut harness) = spawn_harness("mid-turn-injection-runtime-note").await? else {
        eprintln!("Skipping test: nats-server not available");
        return Ok(());
    };

    let prompt_seq = harness
        .append_user(UserRow {
            id: Some("main-prompt"),
            text: "main task",
        })
        .await?;
    harness.invocation_id = admit(&harness, "main-prompt").await?;
    let cursor = Arc::new(AtomicU64::new(prompt_seq));

    // A compaction re-log of a replayed sub-agent runtime note: a user-role
    // row whose id marks it as runtime-authored. It has no prompt admission.
    harness.append(compress_row("summary")).await?;
    let note_id = harnx_core::runtime_note::new_runtime_note_message_id();
    harness
        .append_user(UserRow {
            id: Some(&note_id),
            text: &format!(
                "{}Started sub-agent 'child' in session child-session.",
                harnx_core::runtime_note::RUNTIME_NOTE_PREFIX
            ),
        })
        .await?;

    let callback = harness.callback(Arc::clone(&cursor));
    let mut input = InjectionHarness::input();
    callback(&mut input, &[]).await?;
    assert!(
        input.injected_user_text().is_none(),
        "runtime notes must not be injected as prompts"
    );
    assert!(input.injected_user_messages.is_empty());
    harness.shutdown();
    Ok(())
}

/// The admission boundary stays closed: a durable user row with no identity —
/// not a runtime note, not admitted — still fails the mid-turn injection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idless_non_internal_user_row_is_rejected() -> anyhow::Result<()> {
    require_nextest();
    let Some(mut harness) = spawn_harness("mid-turn-injection-idless-row").await? else {
        eprintln!("Skipping test: nats-server not available");
        return Ok(());
    };

    let prompt_seq = harness
        .append_user(UserRow {
            id: Some("main-prompt"),
            text: "main task",
        })
        .await?;
    harness.invocation_id = admit(&harness, "main-prompt").await?;
    let cursor = Arc::new(AtomicU64::new(prompt_seq));

    harness
        .append_user(UserRow {
            id: None,
            text: "unadmitted input",
        })
        .await?;

    let callback = harness.callback(Arc::clone(&cursor));
    let mut input = InjectionHarness::input();
    let error = callback(&mut input, &[])
        .await
        .expect_err("ID-less non-internal user row must fail the injection");
    assert!(
        error
            .to_string()
            .contains("injected user row has no admission identity"),
        "unexpected error: {error:#}"
    );
    harness.shutdown();
    Ok(())
}

/// The mid-turn rejection carries provenance (log_seq, note-like flag) so an
/// operator can locate the row from the error alone — the id-less row has no
/// id to search for. A user-authored prefix lookalike must report
/// `text=true`; it is still rejected, not misfiled as a runtime note.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idless_rejection_reports_log_seq_and_note_like_flag() -> anyhow::Result<()> {
    require_nextest();
    let Some(mut harness) = spawn_harness("mid-turn-injection-idless-diagnostic").await? else {
        eprintln!("Skipping test: nats-server not available");
        return Ok(());
    };

    let prompt_seq = harness
        .append_user(UserRow {
            id: Some("main-prompt"),
            text: "main task",
        })
        .await?;
    harness.invocation_id = admit(&harness, "main-prompt").await?;
    let cursor = Arc::new(AtomicU64::new(prompt_seq));

    // ID-less row whose text imitates a runtime note.
    harness
        .append_user(UserRow {
            id: None,
            text: &format!(
                "{}Started sub-agent 'child' in session child-session.",
                harnx_core::runtime_note::RUNTIME_NOTE_PREFIX
            ),
        })
        .await?;
    let note_row_seq = prompt_seq + 1;

    let callback = harness.callback(Arc::clone(&cursor));
    let mut input = InjectionHarness::input();
    let error = callback(&mut input, &[])
        .await
        .expect_err("ID-less note-like row must fail the injection");
    let message = error.to_string();
    assert!(
        message.contains("injected user row has no admission identity"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains(&format!("log_seq=Some({note_row_seq})")),
        "diagnostic must name the row's log seq: {message}"
    );
    assert!(
        message.contains("runtime-note-prefixed text=true"),
        "diagnostic must report the prefix lookalike flag: {message}"
    );
    harness.shutdown();
    Ok(())
}
