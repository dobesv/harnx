//! Task 4b wiring tests: session_load is a bounded, read-only, optionally
//! prompt-scoped page over the durable session log. Real NATS fixtures prove
//! the pure-read guarantees: no activation, no resume, no whole-log load.

use super::*;
use crate::nats_session_log::NatsSessionLog;
use crate::nats_session_metadata::SessionMetadataStore;
use crate::nats_test_common::spawn_nats_server;
use crate::nats_worker::subagent_toolset::{SubagentNats, SubagentSessionRoute};
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::SessionLogEntry;
use harnx_toolset::{ToolInvokeError, ToolRequest};
use harnx_toolset_server::invocation_journal::InvocationJournal;
use std::sync::Arc;

fn user(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

fn turn_end(through_seq: u64) -> SessionLogEntry {
    SessionLogEntry::TurnEnd {
        through_seq,
        fence_token: 0,
        timestamp: None,
        usage: None,
    }
}

fn serialized(value: &Value) -> usize {
    serde_json::to_vec(value).expect("serialize result").len()
}

struct LoadFixture {
    /// Owns the nats-server child; dropping it kills the broker, so it stays
    /// bound for the whole test.
    _server: crate::nats_test_common::NatsServerHandle,
    js: async_nats::jetstream::Context,
    toolset: SubagentToolset,
    log: NatsSessionLog,
}

impl LoadFixture {
    const SESSION: &'static str = "load-child";

    async fn new() -> anyhow::Result<Self> {
        let Some(server) = spawn_nats_server().await? else {
            anyhow::bail!("nats-server required for session_load tests")
        };
        let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
        let metadata = SessionMetadataStore::ensure(&js, 1).await?;
        let toolset = SubagentToolset::new(
            "helper",
            SubagentSessionRoute::new("observe", crate::SessionActivationRoute::ClusterShared),
            SubagentNats::new(js.client().clone(), js.clone(), metadata, 1),
        );
        let storage = harnx_core::session_identity::session_key(Some("helper"), Self::SESSION);
        let log = NatsSessionLog::new_with_replicas(js.clone(), storage, 1);
        Ok(Self {
            _server: server,
            js,
            toolset,
            log,
        })
    }

    /// Dispatch one prompt the way Task 2 does, so the prompt has a durable
    /// admission and a logged original for prompt-scoped loads.
    async fn dispatch_prompt(&self) -> anyhow::Result<(String, u64)> {
        let request = ToolRequest {
            operation_id: "operation".into(),
            call_id: "original-prompt".into(),
            tool: "session_send".into(),
            args: json!({"message": "original", "session_id": Self::SESSION}),
            run_context: Some(crate::nats_session::test_support::inherited_tool_context()),
            parent_session_id: Some("trusted-parent".into()),
            parent_agent: None,
            parent_local_session_id: None,
            replay: None,
            tool_call_id: None,
            tool_round: Some(1),
            capabilities: Default::default(),
        };
        let journal = InvocationJournal::ensure(&self.js, 1).await?;
        journal
            .record(&request, ("session_send", "helper", "server"))
            .await?;
        let context = ToolInvocationContext {
            call_id: request.call_id.clone(),
            invoking_session_id: request.parent_session_id.clone(),
            run_context: request.run_context.clone(),
            checkpoint_store: Some(Arc::new(journal.checkpoint_store(&request))),
            partial_result_store: Some(Arc::new(journal.partial_result_store(&request))),
            ..Default::default()
        };
        let (dispatched, wait) = async_session::dispatch::dispatch_send(
            &self.toolset,
            request.args.clone(),
            tokio_util::sync::CancellationToken::new(),
            context,
        )
        .await
        .map_err(|error: ToolInvokeError| anyhow::anyhow!(error.to_string()))?;
        assert!(wait.is_none(), "dispatch-only send carries no initial wait");
        Ok((
            dispatched.handle.prompt_id,
            dispatched.prompt.user_msg_seq(),
        ))
    }

    async fn activation_count(&self) -> anyhow::Result<u64> {
        let mut stream = self.js.get_stream("WORK_NOTIFY_observe").await?;
        Ok(stream.info().await?.state.messages)
    }

    async fn load(&self, args: Value) -> Result<Value, ToolInvokeError> {
        self.toolset.session_load(args).await
    }
}

#[test]
fn load_tool_schema_is_bounded_and_pure() {
    let tools = tool_specs("pkg/helper");
    let load = &tools[4];
    assert_eq!(load.name, "session_load");
    assert_eq!(load.input_schema["required"], json!(["session_id"]));
    assert_eq!(load.input_schema["additionalProperties"], json!(false));
    assert!(
        load.input_schema["properties"]["read"].is_object(),
        "load schema must expose the shared read object"
    );
    assert!(
        load.input_schema["properties"]["prompt_id"].is_object(),
        "load schema must expose prompt_id"
    );
    assert!(load.read_only_hint);
    assert!(
        load.description.contains("Never resumes"),
        "description must state the pure-read guarantee: {}",
        load.description
    );
    assert!(
        load.description.contains("next_index as read.index_min"),
        "description must map the returned next_index to the read.index_min input: {}",
        load.description
    );
    assert!(
        !load
            .description
            .to_lowercase()
            .contains("resume its prior context"),
        "description must not claim context resumption: {}",
        load.description
    );
    // Cancel keeps its original shape.
    let cancel = &tools[5];
    assert!(cancel.input_schema["properties"].get("read").is_none());
    assert!(cancel.input_schema["properties"].get("prompt_id").is_none());
}

#[tokio::test]
async fn default_load_returns_bounded_source_indexed_page_without_side_effects(
) -> anyhow::Result<()> {
    let f = LoadFixture::new().await?;
    for i in 0..5 {
        f.log
            .append_event_async(&user(&format!("entry {i}")))
            .await?;
    }
    let before = f.log.last_entry_async().await?.expect("entries").0;
    let page = f
        .load(json!({ "session_id": LoadFixture::SESSION }))
        .await?;
    assert_eq!(page["session_id"], json!(LoadFixture::SESSION));
    assert!(serialized(&page) <= session_read::DEFAULT_MAX_OUTPUT_BYTES as usize);
    let events = page["events"].as_array().expect("events");
    assert_eq!(events.len(), 5);
    for (i, event) in events.iter().enumerate() {
        assert_eq!(event["index"], json!(i + 1), "source indexes are durable");
        assert_eq!(event["complete"], json!(true));
    }
    assert_eq!(page["truncated"], json!(false));
    assert_eq!(page["next_index"], json!(null));
    assert_eq!(page["scanned_through"], json!(5));
    // Pure read: the durable tail is unchanged by the load.
    let after = f.log.last_entry_async().await?.expect("entries").0;
    assert_eq!(before, after);
    Ok(())
}

#[tokio::test]
async fn load_missing_session_reads_empty_page() -> anyhow::Result<()> {
    let f = LoadFixture::new().await?;
    let page = f.load(json!({ "session_id": "absent-session" })).await?;
    assert_eq!(page["session_id"], json!("absent-session"));
    assert_eq!(page["events"], json!([]));
    assert_eq!(page["truncated"], json!(false));
    assert_eq!(page["next_index"], json!(null));
    assert_eq!(page["scanned_through"], json!(null));
    Ok(())
}

#[tokio::test]
async fn load_rejects_invalid_read_options_and_empty_prompt_id() -> anyhow::Result<()> {
    let f = LoadFixture::new().await?;
    let error = f
        .load(json!({ "session_id": LoadFixture::SESSION, "read": { "text_regex": "[" } }))
        .await
        .expect_err("invalid regex must be rejected");
    assert!(error.to_string().contains("regex"), "unexpected: {error}");
    let error = f
        .load(json!({ "session_id": LoadFixture::SESSION, "prompt_id": "   " }))
        .await
        .expect_err("empty prompt_id must be rejected");
    assert!(error.to_string().contains("prompt_id must not be empty"));
    // Unknown fields are strict errors, not silent options.
    let error = f
        .load(json!({ "session_id": LoadFixture::SESSION, "unexpected": true }))
        .await
        .expect_err("unknown fields must be rejected");
    assert!(error.to_string().contains("unexpected"));
    Ok(())
}

#[tokio::test]
async fn load_recovers_every_omitted_byte_via_continuation() -> anyhow::Result<()> {
    let f = LoadFixture::new().await?;
    // One huge many-line entry: the page can only preview its head and tail,
    // so the middle must be recoverable byte-exactly.
    let body = std::iter::repeat_n("z".repeat(30), 2_000)
        .collect::<Vec<_>>()
        .join("\n");
    let entry = user(&body);
    let expected = session_read::render_event_text(&entry);
    f.log.append_event_async(&entry).await?;
    let page = f
        .load(json!({ "session_id": LoadFixture::SESSION, "read": { "max_output_bytes": 2048 } }))
        .await?;
    assert!(serialized(&page) <= 2048);
    let events = page["events"].as_array().expect("events");
    assert_eq!(events[0]["complete"], json!(false));
    let omitted = page["omitted_ranges"].as_array().expect("omitted ranges");
    assert!(!omitted.is_empty(), "giant entry reports what was cut");
    // The delivered head plus every recovered range reconstructs the exact
    // canonical rendering: no gaps, no duplicates.
    let delivered = events[0]["text"].as_str().expect("text").to_string();
    let mut recovered = String::new();
    let mut offset = omitted[0]["start"].as_u64().expect("start");
    loop {
        let page = f
            .load(json!({
                "session_id": LoadFixture::SESSION,
                "read": { "entry_index": 1, "byte_offset": offset, "max_output_bytes": 2048 }
            }))
            .await?;
        let text = page["text"].as_str().expect("continuation text");
        recovered.push_str(text);
        match page["continuation"].as_object() {
            Some(next) => offset = next["byte_offset"].as_u64().expect("next offset"),
            None => break,
        }
    }
    assert_eq!(format!("{delivered}{recovered}"), expected);
    Ok(())
}

#[tokio::test]
async fn continuation_offset_must_land_on_utf8_boundary() -> anyhow::Result<()> {
    let f = LoadFixture::new().await?;
    let entry = user(&"é".repeat(2_000));
    f.log.append_event_async(&entry).await?;
    // Byte offsets refer to the canonical rendered text; land one byte into
    // the first two-byte character, where a cut would split it.
    let rendered = session_read::render_event_text(&entry);
    let mid_character = rendered.find('é').expect("rendered text has content") as u64 + 1;
    let error = f
        .load(json!({
            "session_id": LoadFixture::SESSION,
            "read": { "entry_index": 1, "byte_offset": mid_character }
        }))
        .await
        .expect_err("mid-character offsets are validation errors");
    assert!(error.to_string().contains("UTF-8"), "unexpected: {error}");
    Ok(())
}

#[tokio::test]
async fn prompt_scoped_load_excludes_later_turns_and_never_activates() -> anyhow::Result<()> {
    let f = LoadFixture::new().await?;
    let (prompt_id, prompt_seq) = f.dispatch_prompt().await?;
    f.log
        .append_event_async(&SessionLogEntry::Message {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::Text("original answer".into()),
            timestamp: None,
            fence_token: None,
        })
        .await?;
    let terminal = f.log.append_event_async(&turn_end(prompt_seq)).await?;
    // A later, unrelated turn must stay invisible to the prompt-scoped page.
    let later = f
        .log
        .append_event_async(&user("unrelated later turn"))
        .await?;
    f.log.append_event_async(&turn_end(later)).await?;

    let before = f.activation_count().await?;
    let page = f
        .load(json!({ "session_id": LoadFixture::SESSION, "prompt_id": prompt_id }))
        .await?;
    assert_eq!(page["prompt_id"], json!(prompt_id));
    let events = page["events"].as_array().expect("events");
    let indexes: Vec<u64> = events
        .iter()
        .map(|e| e["index"].as_u64().unwrap())
        .collect();
    assert_eq!(indexes, vec![prompt_seq, prompt_seq + 1, terminal]);
    assert!(
        !serde_json::to_string(&page)
            .unwrap()
            .contains("unrelated later turn"),
        "later turns are out of scope"
    );
    assert_eq!(page["effective_index_max"], json!(terminal));
    assert_eq!(page["scanned_through"], json!(terminal));
    assert_eq!(page["truncated"], json!(false));
    assert_eq!(page["next_index"], json!(null));
    // Prompt-scoped load is a pure read: nothing was activated or repaired.
    assert_eq!(f.activation_count().await?, before);
    // The unscoped page still sees the whole session.
    let whole = f
        .load(json!({ "session_id": LoadFixture::SESSION }))
        .await?;
    let events = whole["events"].as_array().expect("events");
    assert_eq!(events.len(), 5);
    Ok(())
}

#[tokio::test]
async fn prompt_scoped_load_reports_status_and_terminal() -> anyhow::Result<()> {
    let f = LoadFixture::new().await?;
    let (prompt_id, prompt_seq) = f.dispatch_prompt().await?;
    let args = json!({ "session_id": LoadFixture::SESSION, "prompt_id": prompt_id });
    let activations = f.activation_count().await?;
    let log_len = f.log.load_events_latest_async().await?.len();
    let pending = f.load(args.clone()).await?;
    // A pending prompt with an unconfirmed dispatch is only observed: load
    // never repairs, activates or appends.
    assert_eq!(f.activation_count().await?, activations);
    assert_eq!(f.log.load_events_latest_async().await?.len(), log_len);
    assert_eq!(
        (&pending["status"], &pending["terminal"]),
        (&json!("pending"), &json!(false))
    );
    f.log.append_event_async(&turn_end(prompt_seq)).await?;
    let done = f.load(args).await?;
    assert_eq!(
        (&done["status"], &done["terminal"]),
        (&json!("completed"), &json!(true))
    );
    let plain = f
        .load(json!({ "session_id": LoadFixture::SESSION }))
        .await?;
    assert!(plain.get("status").is_none() && plain.get("terminal").is_none());
    Ok(())
}

#[tokio::test]
async fn prompt_scoped_load_fails_closed_for_unknown_prompts() -> anyhow::Result<()> {
    let f = LoadFixture::new().await?;
    let (prompt_id, _seq) = f.dispatch_prompt().await?;
    let error = f
        .load(json!({ "session_id": LoadFixture::SESSION, "prompt_id": "never-dispatched" }))
        .await
        .expect_err("unknown prompt must fail");
    assert!(
        error.to_string().contains("not a dispatched prompt"),
        "unexpected: {error}"
    );
    // The real prompt still loads; a session_id/prompt_id mix-up is caught by
    // the namespace check, not silently ignored.
    let page = f
        .load(json!({ "session_id": LoadFixture::SESSION, "prompt_id": prompt_id }))
        .await?;
    assert_eq!(page["prompt_id"], json!(prompt_id));
    Ok(())
}
