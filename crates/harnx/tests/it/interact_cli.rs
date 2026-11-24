//! `harnx interact list/answer` against a real NATS server and durable
//! acceptance. Only the worker side is stubbed: a test responder on the
//! session control subject runs the real `accept_interaction_response`
//! against a real lease (or replies with a forced outcome).

use anyhow::{Context, Result};
use futures_util::StreamExt;
use harnx_core::session::{SessionLogEntry, ToolOutput};
use harnx_runtime::nats_session_log::NatsSessionLog;
use harnx_runtime::nats_session_metadata::{
    SessionInitializer, SessionMetadata, SessionMetadataStore,
};
use harnx_runtime::nats_worker::{control_subject, ControlCommand, NatsSessionLogBackend};
use harnx_runtime::{
    interactions::{accept_interaction_response, InteractionDecisionOutcome},
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    surfaces::{SurfaceActionOutcome, SurfaceActionReply},
};
use harnx_test_bins::NatsServerHandle;
use serde_json::{json, Value};
use std::{process::Output, sync::Arc, time::Duration};

use crate::cli_runner::run_harnx;

const AGENT: &str = "plain";
const SESSION: &str = "history";
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The agent reference the CLI accepts (`agent@cluster`).
fn agent_ref() -> String {
    format!("{AGENT}@fixture")
}

fn a2ui_surface() -> Value {
    json!({
        "version": "1.0",
        "createSurface": {
            "surfaceId": "deploy-1",
            "dataModel": {"choice": ["staging"], "name": "rel-1"},
            "components": [
                {"id": "root", "component": "Card", "child": "col"},
                {"id": "col", "component": "Column",
                 "children": ["title", "choice", "name", "go_label", "go"]},
                {"id": "title", "component": "Text", "text": "Pick a deployment target"},
                {"id": "choice", "component": "ChoicePicker",
                 "options": [{"label": "Staging", "value": "staging"},
                             {"label": "Production", "value": "prod"}],
                 "value": {"@path": "/choice"}},
                {"id": "name", "component": "TextField", "label": "Release name",
                 "value": {"@path": "/name"}},
                {"id": "go_label", "component": "Text", "text": "Deploy"},
                {"id": "go", "component": "Button", "child": "go_label",
                 "action": {"event": {"name": "deploy"}}}
            ]
        }
    })
}

fn surface_action_surface() -> Value {
    let mut payload = a2ui_surface();
    let components = payload["createSurface"]["components"]
        .as_array_mut()
        .expect("surface components");
    let deploy = components
        .iter_mut()
        .find(|component| component["id"] == "go")
        .expect("deploy button");
    deploy["metadata"] = json!({"extensions": {"harnx": {"visibility": "note"}}});
    components.push(json!({"id": "hold_label", "component": "Text", "text": "Hold"}));
    components.push(json!({
        "id": "hold",
        "component": "Button",
        "child": "hold_label",
        "metadata": {"extensions": {"harnx": {"visibility": "note"}}},
        "action": {"event": {"name": "hold"}}
    }));
    payload
}

fn entry(value: Value) -> SessionLogEntry {
    serde_json::from_value(value).expect("valid log entry")
}

fn interaction_requested(round: u64, id: &str, kind: &str, payload: Value) -> SessionLogEntry {
    entry(json!({
        "type": "interaction_requested",
        "tool_round_seq": round,
        "tool_call_id": "call-1",
        "interaction_id": id,
        "producer": "middleware:test",
        "step_path": [],
        "key": "form",
        "kind": kind,
        "payload": payload,
        "summary": "Pick a deployment target",
        "chain_version": "v1",
        "checkpoint": "cp",
        "fence_token": 10,
    }))
}

fn suspension_ready(round: u64, pending: &[&str], fence: u64) -> SessionLogEntry {
    entry(json!({
        "type": "suspension_ready",
        "tool_round_seq": round,
        "pending": pending,
        "fence_token": fence,
    }))
}

struct Fixture {
    root: tempfile::TempDir,
    log: NatsSessionLog,
    lease: Arc<NatsSessionLease>,
    force_outcomes: tokio::sync::mpsc::UnboundedSender<InteractionDecisionOutcome>,
    // Holds the spawned nats-server open for the fixture's lifetime; dropping
    // the handle kills the server and every connection to it.
    _server: NatsServerHandle,
}

impl Fixture {
    /// Spawn a NATS server, a leased session log, and the control responder.
    /// Returns `None` when no NATS server binary is available (CI skip).
    async fn new() -> Result<Option<Self>> {
        let Some((server, root, client)) = start_fixture_server().await? else {
            return Ok(None);
        };
        let (log, lease, backend, storage_key) = create_fixture_session(&client).await?;
        let force_outcomes =
            install_control_responder(client, storage_key, backend, lease.clone()).await?;
        Ok(Some(Self {
            root,
            log,
            lease,
            force_outcomes,
            _server: server,
        }))
    }

    async fn publish_surface(&self, payload: Value) -> Result<()> {
        self.log
            .append_event_async(&SessionLogEntry::ToolResults {
                results: vec![ToolOutput {
                    id: Some("tool-publish".into()),
                    name: "publish_surface".into(),
                    output: json!({}),
                    markdown: None,
                    content: Vec::new(),
                    user_surfaces: vec![payload],
                    switch_agent: None,
                }],
                handoff_selection: None,
                timestamp: None,
            })
            .await?;
        Ok(())
    }

    async fn seed_round(&self, requests: &[SessionLogEntry]) -> Result<()> {
        let fence = self.lease.fence_token();
        let round = self
            .log
            .append_event_async(&entry(json!({
                "type": "tool_calls",
                "text": "",
                "calls": [{
                    "name": "counter_run",
                    "arguments": {},
                    "id": "call-1"
                }],
                "fence_token": fence,
            })))
            .await?;
        for request in requests {
            self.log.append_event_async(request).await?;
        }
        let pending: Vec<String> = requests
            .iter()
            .map(|request| match request {
                SessionLogEntry::InteractionRequested { interaction_id, .. } => {
                    interaction_id.clone()
                }
                other => panic!("unexpected entry: {other:?}"),
            })
            .collect();
        self.log
            .append_event_async(&suspension_ready(
                round,
                &pending.iter().map(String::as_str).collect::<Vec<_>>(),
                fence,
            ))
            .await?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct ControlReplyContext<'a> {
    client: &'a async_nats::Client,
    backend: &'a NatsSessionLogBackend,
    lease: &'a Arc<NatsSessionLease>,
}

struct InteractionReply {
    id: String,
    request_seq: u64,
    response: harnx_core::interaction::InteractionResponse,
}

async fn start_fixture_server(
) -> Result<Option<(NatsServerHandle, tempfile::TempDir, async_nats::Client)>> {
    let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(None);
    };
    let root = tempfile::tempdir()?;
    let servers = root.path().join("config/nats_servers");
    std::fs::create_dir_all(&servers)?;
    std::fs::write(
        servers.join("fixture.yaml"),
        format!("url: {}\n", server.url),
    )?;
    let client = async_nats::connect(&server.url).await?;
    Ok(Some((server, root, client)))
}

async fn create_fixture_session(
    client: &async_nats::Client,
) -> Result<(
    NatsSessionLog,
    Arc<NatsSessionLease>,
    NatsSessionLogBackend,
    String,
)> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let metadata = SessionMetadata::new(
        SESSION,
        SessionInitializer::named(AGENT, Default::default()),
    );
    SessionMetadataStore::ensure(&jetstream, 1)
        .await?
        .create(&metadata)
        .await?;
    let storage_key = metadata.storage_key();
    let log = NatsSessionLog::new_with_replicas(jetstream.clone(), storage_key.clone(), 1);
    let lease = acquire_fixture_lease(jetstream.clone(), &storage_key).await?;
    let backend = NatsSessionLogBackend::new(jetstream, storage_key.clone(), 1);
    Ok((log, lease, backend, storage_key))
}

async fn acquire_fixture_lease(
    jetstream: async_nats::jetstream::Context,
    storage_key: &str,
) -> Result<Arc<NatsSessionLease>> {
    NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream,
        session_id: storage_key,
        worker_id: "interact-cli-test".into(),
        generation: 1,
        config: NatsLeaseConfig::default(),
        session_metadata: None,
    })
    .await?
    .context("acquire fixture lease")
    .map(Arc::new)
}

async fn install_control_responder(
    client: async_nats::Client,
    storage_key: String,
    backend: NatsSessionLogBackend,
    lease: Arc<NatsSessionLease>,
) -> Result<tokio::sync::mpsc::UnboundedSender<InteractionDecisionOutcome>> {
    let commands = client.subscribe(control_subject(&storage_key)).await?;
    client.flush().await?;
    let (force_outcomes, forced) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(control_responder(client, backend, lease, commands, forced));
    Ok(force_outcomes)
}

async fn control_responder(
    client: async_nats::Client,
    backend: NatsSessionLogBackend,
    lease: Arc<NatsSessionLease>,
    mut commands: async_nats::Subscriber,
    mut forced: tokio::sync::mpsc::UnboundedReceiver<InteractionDecisionOutcome>,
) {
    let context = ControlReplyContext {
        client: &client,
        backend: &backend,
        lease: &lease,
    };
    while let Some(message) = commands.next().await {
        handle_control_message(context, &mut forced, message).await;
    }
}

async fn handle_control_message(
    context: ControlReplyContext<'_>,
    forced: &mut tokio::sync::mpsc::UnboundedReceiver<InteractionDecisionOutcome>,
    message: async_nats::Message,
) {
    let Ok(command) = ControlCommand::from_bytes(&message.payload) else {
        return;
    };
    let Some(reply) = message.reply else {
        return;
    };
    match command {
        ControlCommand::InteractionResponse {
            interaction_id,
            request_seq,
            response,
        } => {
            reply_interaction(
                context,
                forced,
                reply,
                InteractionReply {
                    id: interaction_id,
                    request_seq,
                    response,
                },
            )
            .await;
        }
        ControlCommand::SurfaceAction { submission } => {
            reply_surface_action(context, reply, submission).await;
        }
        _ => {}
    }
}

async fn reply_interaction(
    context: ControlReplyContext<'_>,
    forced: &mut tokio::sync::mpsc::UnboundedReceiver<InteractionDecisionOutcome>,
    reply: async_nats::Subject,
    interaction: InteractionReply,
) {
    let outcome = match forced.try_recv() {
        Ok(outcome) => outcome,
        Err(_) => accept_interaction_response(
            context.backend,
            context.lease,
            &interaction.id,
            interaction.request_seq,
            &interaction.response,
        )
        .await
        .expect("accept interaction response"),
    };
    publish_control_reply(context.client, reply, &outcome, "control reply").await;
}

async fn reply_surface_action(
    context: ControlReplyContext<'_>,
    reply: async_nats::Subject,
    submission: harnx_runtime::surfaces::SurfaceActionSubmission,
) {
    let pending = SurfaceActionReply::rejected(SurfaceActionOutcome::Pending);
    publish_control_reply(
        context.client,
        reply.clone(),
        &pending,
        "pending surface-action reply",
    )
    .await;
    let answer = apply_fixture_surface_action(context, &submission).await;
    publish_control_reply(context.client, reply, &answer, "surface-action reply").await;
}

async fn apply_fixture_surface_action(
    context: ControlReplyContext<'_>,
    submission: &harnx_runtime::surfaces::SurfaceActionSubmission,
) -> SurfaceActionReply {
    let entries = context
        .backend
        .load_events_latest_async()
        .await
        .expect("load fixture session log");
    let declaration =
        match harnx_runtime::surfaces::validate_surface_submission(&entries, submission) {
            Ok(declaration) => declaration,
            Err(outcome) => return SurfaceActionReply::rejected(outcome),
        };
    let text = harnx_core::interaction::non_modal::action_text(
        &submission.response.action,
        &submission.response.data,
        submission.user_message.as_deref(),
        None,
    );
    let visible_text = (declaration.visibility
        != harnx_core::interaction::non_modal::SurfaceVisibility::Invisible)
        .then_some(text.as_str());
    context
        .backend
        .append_event(&harnx_runtime::surfaces::surface_action_entry(
            submission,
            &declaration,
            visible_text,
            context.lease.fence_token(),
        ))
        .await
        .expect("append fixture surface action");
    SurfaceActionReply {
        outcome: SurfaceActionOutcome::Applied,
        turn_text: (declaration.visibility
            == harnx_core::interaction::non_modal::SurfaceVisibility::Turn)
            .then_some(text),
    }
}

async fn publish_control_reply<T: serde::Serialize>(
    client: &async_nats::Client,
    reply: async_nats::Subject,
    payload: &T,
    expectation: &str,
) {
    client
        .publish(
            reply,
            serde_json::to_vec(payload)
                .expect("control reply serializable")
                .into(),
        )
        .await
        .unwrap_or_else(|error| panic!("publish {expectation}: {error}"));
}

/// The CLI runs with colors off so the assertions match plain output.
const CLI_ENV: &[(&str, &str)] = &[("NO_COLOR", "1")];

/// Run `harnx interact list <agent> <session>`.
fn list(f: &Fixture) -> Result<Output> {
    let agent = agent_ref();
    run_harnx(
        f.root.path(),
        &["interact", "list", &agent, SESSION],
        CLI_ENV,
    )
    .context("run interact list")
}

/// Run `harnx interact answer` for one interaction, with KEY=VALUE data pairs
/// and an optional note.
fn answer(
    f: &Fixture,
    id: &str,
    action: &str,
    data: &[&str],
    note: Option<&str>,
) -> Result<Output> {
    let agent = agent_ref();
    let mut args = vec![
        "interact", "answer", &agent, SESSION, "--id", id, "--action", action,
    ];
    for pair in data {
        args.push("--data");
        args.push(pair);
    }
    if let Some(note) = note {
        args.push("--note");
        args.push(note);
    }
    run_harnx(f.root.path(), &args, CLI_ENV).context("run interact answer")
}

/// The response journaled for one interaction, if any.
async fn journaled_response(
    log: &NatsSessionLog,
    interaction_id: &str,
) -> Result<Option<harnx_core::interaction::InteractionResponse>> {
    for (_, entry) in log.load_events_latest_async().await? {
        if let SessionLogEntry::InteractionResponse {
            interaction_id: id,
            response,
            ..
        } = entry
        {
            if id == interaction_id {
                return Ok(Some(response));
            }
        }
    }
    Ok(None)
}

async fn count_responses(f: &Fixture, interaction_id: &str) -> Result<usize> {
    let mut count = 0;
    for (_, entry) in f.log.load_events_latest_async().await? {
        if let SessionLogEntry::InteractionResponse {
            interaction_id: id, ..
        } = entry
        {
            if id == interaction_id {
                count += 1;
            }
        }
    }
    Ok(count)
}

/// Poll the durable log until the interaction's response lands.
async fn wait_for_response(
    f: &Fixture,
    interaction_id: &str,
) -> Result<harnx_core::interaction::InteractionResponse> {
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if let Some(response) = journaled_response(&f.log, interaction_id).await? {
                return Ok(response);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("response was not journaled in time")?
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// Assert the CLI run failed and its stderr explains why.
fn assert_failed_with(output: &Output, expected: &str) {
    assert!(!output.status.success(), "stderr: {}", stderr(output));
    assert!(
        stderr(output).contains(expected),
        "stderr: {}",
        stderr(output)
    );
}

/// Run an answer that must be rejected, and assert its stderr says why.
fn assert_answer_rejected(f: &Fixture, id: &str, action: &str, data: &[&str], expected: &str) {
    let output = answer(f, id, action, data, None).expect("run interact answer");
    assert_failed_with(&output, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interact_list_renders_pending_interactions_as_text() -> Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    f.seed_round(&[
        interaction_requested(1, "int-form", "a2ui", a2ui_surface()),
        interaction_requested(
            1,
            "int-confirm",
            "confirm",
            json!({"message": "Deploy to production?"}),
        ),
    ])
    .await?;
    let output = list(&f)?;
    let text = stdout(&output);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    // ToolCalls is sequence 1, so the first request lands at sequence 2.
    assert!(
        text.contains("Pending interaction int-form (kind a2ui, request sequence 2)"),
        "{text}"
    );
    assert!(text.contains("Pick a deployment target"), "{text}");
    assert!(text.contains("Release name"), "{text}");
    assert!(text.contains("Staging"), "{text}");
    assert!(text.contains("deploy"), "{text}");
    assert!(text.contains("Deploy to production?"), "{text}");
    assert!(
        text.contains(&format!(
            "harnx interact answer {} {} --id",
            agent_ref(),
            SESSION
        )),
        "answer hint missing: {text}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn surface_list_renders_published_surfaces_from_session_log() -> Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    f.publish_surface(a2ui_surface()).await?;

    let agent = agent_ref();
    let output = run_harnx(
        f.root.path(),
        &["surface", "list", &agent, SESSION],
        CLI_ENV,
    )
    .context("run surface list")?;
    let text = stdout(&output);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(text.contains("Surface deploy-1"), "{text}");
    assert!(text.contains("published at sequence 1"), "{text}");
    assert!(text.contains("outstanding"), "{text}");
    assert!(text.contains("Pick a deployment target"), "{text}");
    assert!(text.contains("Act with: harnx surface act"), "{text}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn surface_act_submits_action_and_maps_applied_and_conflicting_exit_codes() -> Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    f.publish_surface(surface_action_surface()).await?;

    let agent = agent_ref();
    let applied = run_harnx(
        f.root.path(),
        &[
            "surface",
            "act",
            &agent,
            SESSION,
            "--id",
            "deploy-1",
            "--publish-seq",
            "1",
            "--action",
            "deploy",
            "--data",
            "choice=Production",
            "--data",
            "name=rel-2",
            "--note",
            "deploy rel-2",
        ],
        CLI_ENV,
    )
    .context("run surface act for accepted action")?;
    assert!(applied.status.success(), "stderr: {}", stderr(&applied));
    assert!(
        stdout(&applied).contains("Accepted"),
        "{}",
        stdout(&applied)
    );

    let entries = f.log.load_events_latest_async().await?;
    let action = entries.iter().find_map(|(_, entry)| match entry {
        SessionLogEntry::SurfaceAction {
            surface_id,
            publish_seq,
            action,
            data,
            user_message,
            visibility,
            ..
        } => Some((
            surface_id,
            publish_seq,
            action,
            data,
            user_message,
            visibility,
        )),
        _ => None,
    });
    let Some((surface_id, publish_seq, action, data, user_message, visibility)) = action else {
        anyhow::bail!("surface act did not append a SurfaceAction entry");
    };
    assert_eq!(surface_id, "deploy-1");
    assert_eq!(*publish_seq, 1);
    assert_eq!(action, "deploy");
    assert_eq!(data["choice"], json!(["prod"]));
    assert_eq!(data["name"], "rel-2");
    assert_eq!(user_message.as_deref(), Some("deploy rel-2"));
    assert_eq!(visibility, "note");

    let conflicting = run_harnx(
        f.root.path(),
        &[
            "surface",
            "act",
            &agent,
            SESSION,
            "--id",
            "deploy-1",
            "--publish-seq",
            "1",
            "--action",
            "hold",
            "--data",
            "choice=Production",
            "--data",
            "name=rel-2",
        ],
        CLI_ENV,
    )
    .context("run surface act for conflicting action")?;
    assert_eq!(conflicting.status.code(), Some(1));
    assert!(
        stderr(&conflicting).contains("conflicting"),
        "{}",
        stderr(&conflicting)
    );
    Ok(())
}

/// Spawn a fixture with one pending `int-form` a2ui interaction.
async fn form_fixture() -> Result<Option<Fixture>> {
    let Some(f) = Fixture::new().await? else {
        return Ok(None);
    };
    f.seed_round(&[interaction_requested(1, "int-form", "a2ui", a2ui_surface())])
        .await?;
    Ok(Some(f))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interact_answer_accepts_data_and_is_idempotent() -> Result<()> {
    let Some(f) = form_fixture().await? else {
        return Ok(());
    };
    let output = answer(
        &f,
        "int-form",
        "deploy",
        &["choice=Production", "name=rel-2"],
        Some("ship it"),
    )?;
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(
        stdout(&output).contains("Accepted"),
        "stdout: {}",
        stdout(&output)
    );
    let response = wait_for_response(&f, "int-form").await?;
    assert_eq!(response.action, "deploy");
    assert_eq!(
        response.data,
        serde_json::Map::from_iter([
            ("choice".to_string(), json!(["prod"])),
            ("name".to_string(), json!("rel-2")),
        ])
    );
    assert_eq!(response.note.as_deref(), Some("ship it"));

    // Identical retry is acknowledged without a second append.
    let retry = answer(
        &f,
        "int-form",
        "deploy",
        &["choice=Production", "name=rel-2"],
        Some("ship it"),
    )?;
    assert!(retry.status.success(), "stderr: {}", stderr(&retry));
    assert!(
        stdout(&retry).contains("Already applied"),
        "stdout: {}",
        stdout(&retry)
    );
    let responses = count_responses(&f, "int-form").await?;
    assert_eq!(responses, 1, "identical retry must not append");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interact_answer_invalid_is_rejected_without_journaling_then_corrects() -> Result<()> {
    let Some(f) = form_fixture().await? else {
        return Ok(());
    };
    assert_answer_rejected(&f, "int-form", "rollback", &[], "not declared");
    assert_answer_rejected(&f, "int-form", "deploy", &["choice=canary"], "canary");
    assert_eq!(
        count_responses(&f, "int-form").await?,
        0,
        "nothing journaled"
    );
    // The corrected answer still works: no decision was recorded.
    let corrected = answer(&f, "int-form", "deploy", &["choice=Staging"], None)?;
    assert!(corrected.status.success(), "stderr: {}", stderr(&corrected));
    let response = journaled_response(&f.log, "int-form").await?;
    assert_eq!(response.map(|r| r.action), Some("deploy".to_string()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interact_answer_deny_dismisses_and_confirm_approves() -> Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    f.seed_round(&[
        interaction_requested(1, "int-cancel", "a2ui", a2ui_surface()),
        interaction_requested(
            1,
            "int-confirm",
            "confirm",
            json!({"message": "Deploy to production?"}),
        ),
    ])
    .await?;
    let deny = answer(&f, "int-cancel", "deny", &[], None)?;
    assert!(deny.status.success(), "stderr: {}", stderr(&deny));
    let response = journaled_response(&f.log, "int-cancel").await?;
    let response = response.expect("deny journaled");
    assert_eq!(response.action, "deny");
    assert!(response.data.is_empty());

    let approve = answer(&f, "int-confirm", "approve", &[], None)?;
    assert!(approve.status.success(), "stderr: {}", stderr(&approve));
    let response = journaled_response(&f.log, "int-confirm")
        .await?
        .expect("approve journaled");
    assert_eq!(response.action, "approve");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interact_answer_reports_stale_and_conflicting_outcomes() -> Result<()> {
    let Some(f) = form_fixture().await? else {
        return Ok(());
    };
    // Unknown id never reaches the worker.
    assert_answer_rejected(&f, "int-missing", "deny", &[], "not pending");

    // The worker-side outcome decides the message when the request is pending.
    f.force_outcomes.send(InteractionDecisionOutcome::Stale)?;
    assert_answer_rejected(&f, "int-form", "deploy", &[], "stale");

    f.force_outcomes
        .send(InteractionDecisionOutcome::Conflicting)?;
    assert_answer_rejected(&f, "int-form", "deploy", &[], "conflicting");
    assert_eq!(
        count_responses(&f, "int-form").await?,
        0,
        "no forced outcome is journaled by the responder"
    );
    Ok(())
}
