//! #2555: a tool reply that passed the tool server's JSON size bound can
//! still overflow the worker's YAML `ToolResults` session-log entry. The
//! broker ceiling is lowered here (`max_payload: 64 KiB`) so the same
//! overflow reproduces with small fixtures: the reply fits the tool server's
//! budget, but the serialized entry does not fit the worker's.

use super::result_fixture::{CapturedProvider, ProviderSession, ResultEnvironment, SessionSpec};
use super::*;

use anyhow::Context;
use async_trait::async_trait;
use harnx_core::session::ToolOutput;
use harnx_toolset::{ToolInvokeError, ToolSpec, Toolset};
use harnx_toolset_server::serve_over_nats;
use serde_json::Value;
use tokio_util::task::AbortOnDropHandle;

#[path = "tool_result_entry_budget/orphan.rs"]
mod orphan;
/// Broker ceiling used by every test in this module.
const MAX_PAYLOAD: usize = 64 * 1024;
/// Mirrors the worker's envelope headroom (tool_result_budget.rs).
const HEADROOM: usize = 1024;

/// A pretty-printed JSON listing — the shape of the #2555 staging failure.
/// Short rows maximize the YAML/JSON size ratio: each line costs one escaped
/// `\n` (two bytes) in the reply's JSON, but a newline plus the entry's
/// block-scalar indentation in the session log, so the reply fits the tool
/// server's per-reply bound while the entry overflows the broker ceiling.
fn bloat_listing() -> Value {
    let plans: Vec<String> = (0..4_000).map(|i| format!("{:04}", i)).collect();
    json!({
        "listing": serde_json::to_string_pretty(&json!({ "plans": plans })).unwrap()
    })
}

fn sum_output(bytes: usize) -> Value {
    json!({ "out": "y".repeat(bytes) })
}

fn tool_output(tool: &str) -> Value {
    match tool {
        "bloat_listing" => bloat_listing(),
        "sum_large" => sum_output(30_000),
        "sum_medium_a" | "sum_medium_b" => sum_output(18_000),
        other => panic!("unexpected tool {other}"),
    }
}

#[derive(Clone, Default)]
struct EntryBudgetTools(Arc<AtomicUsize>);

#[async_trait]
impl Toolset for EntryBudgetTools {
    fn name(&self) -> &str {
        "entry_budget"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        ["bloat_listing", "sum_large", "sum_medium_a", "sum_medium_b"]
            .into_iter()
            .map(|name| ToolSpec {
                name: name.into(),
                description: "Return entry-budget fixture output".into(),
                input_schema: json!({"type": "object"}),
                cancellation_guarantee: Default::default(),
                idempotent_hint: true,
                read_only_hint: true,
                timeout_secs: None,
                meta: None,
            })
            .collect()
    }

    async fn invoke(
        &self,
        tool: &str,
        _: Value,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(tool_output(tool))
    }
}

struct EntryBudgetFixture {
    env: ResultEnvironment,
    _native: AbortOnDropHandle<Result<()>>,
    calls: Arc<AtomicUsize>,
}

impl EntryBudgetFixture {
    async fn start() -> Result<Self> {
        let env = ResultEnvironment::start_with_broker_config(Some("max_payload: 65536\n")).await?;
        let tools = EntryBudgetTools::default();
        let calls = tools.0.clone();
        let (url, scope) = (env.broker.url().to_string(), env.scope.clone());
        let native = AbortOnDropHandle::new(tokio::spawn(async move {
            serve_over_nats(tools, scope, &url, "").await
        }));
        Ok(Self {
            env,
            _native: native,
            calls,
        })
    }

    async fn session(&self, api_base: &str, id: &str) -> Result<ProviderSession> {
        ProviderSession::create(
            &self.env,
            SessionSpec {
                id,
                api_base,
                supports_vision: false,
            },
        )
        .await
    }
}

fn tool_results_entry(entries: &[(u64, SessionLogEntry)]) -> Result<&SessionLogEntry> {
    entries
        .iter()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::ToolResults { .. } => Some(entry),
            _ => None,
        })
        .context("ToolResults entry")
}

fn result_for<'a>(results: &'a [ToolOutput], id: &str) -> &'a ToolOutput {
    results
        .iter()
        .find(|result| result.id.as_deref() == Some(id))
        .unwrap_or_else(|| panic!("missing tool result {id}"))
}

fn attachment_url(text: &str) -> &str {
    let start = text
        .find("cid:media:")
        .expect("retained original reference");
    let tail = &text[start..];
    tail.split_whitespace()
        .next()
        .unwrap()
        .trim_end_matches(['.', ')', ']', '>'])
}

/// Assert a durable entry fits the broker ceiling the worker budgeted for.
fn assert_entry_fits_broker(entry: &SessionLogEntry) -> Result<()> {
    let len = harnx_runtime::nats_session_log::serialize_entry(entry)?.len();
    assert!(
        len + HEADROOM <= MAX_PAYLOAD,
        "serialized entry is {len} bytes, budget is {}",
        MAX_PAYLOAD - HEADROOM
    );
    Ok(())
}

/// Assert the output was degraded into the recoverable size error, and return
/// the `cid:media:` URL its original was retained at.
async fn assert_degraded_with_retained_original(
    output: &Value,
    js: &async_nats::jetstream::Context,
) -> Result<String> {
    assert_eq!(output["is_error"], true, "{output}");
    let error = output["error"].as_str().unwrap();
    assert!(
        error.contains("exceeded storage/transport size limit"),
        "{error}"
    );
    assert!(
        error.contains("Read with attachment_read using byte_offset/byte_limit."),
        "{error}"
    );
    let url = attachment_url(error).to_string();
    let (original, mime) = harnx_runtime::nats_attachments::get_session_attachment(js, 1, &url)
        .await?
        .with_context(|| format!("retained original at {url}"))?;
    assert_eq!(mime, "application/json");
    let original: ToolOutput = serde_json::from_slice(&original)?;
    assert!(
        !matches!(original.output["listing"], Value::Null)
            || original.output["out"]
                .as_str()
                .is_some_and(|s| s.len() > 20_000),
        "attachment must hold the original output, got {}",
        original.output
    );
    Ok(url)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yaml_bloated_reply_degrades_and_turn_completes() -> Result<()> {
    require_nextest();
    let fixture = EntryBudgetFixture::start().await?;
    let provider = CapturedProvider::start(
        &[("bloat_listing", "call-bloat")],
        "bounded entry round complete",
    )
    .await?;
    let session = fixture.session(&provider.base, "entry1").await?;

    // Before the fix this append was rejected by the broker and the turn failed.
    session.run_turn(None).await?;

    let requests = provider.finish().await?;
    assert_eq!(
        requests.len(),
        2,
        "the turn must continue after the bounded append"
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);

    let log = NatsSessionLog::new_with_replicas(fixture.env.js.clone(), session.key.clone(), 1);
    let entries = log.load_events_async().await?;
    let entry = tool_results_entry(&entries)?;
    assert_entry_fits_broker(entry)?;
    let SessionLogEntry::ToolResults { results, .. } = entry else {
        unreachable!("matched ToolResults above")
    };
    assert_degraded_with_retained_original(&results[0].output, &fixture.env.js).await?;
    let original: ToolOutput = serde_json::from_slice(
        &harnx_runtime::nats_attachments::get_session_attachment(
            &fixture.env.js,
            1,
            attachment_url(results[0].output["error"].as_str().unwrap()),
        )
        .await?
        .context("original")?
        .0,
    )?;
    assert_eq!(
        original.output,
        bloat_listing(),
        "attachment holds the original"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sum_of_fitting_results_degrades_only_the_biggest() -> Result<()> {
    require_nextest();
    let fixture = EntryBudgetFixture::start().await?;
    let provider = CapturedProvider::start(
        &[
            ("sum_large", "call-large"),
            ("sum_medium_a", "call-a"),
            ("sum_medium_b", "call-b"),
        ],
        "sum round complete",
    )
    .await?;
    let session = fixture.session(&provider.base, "entry2").await?;

    session.run_turn(None).await?;

    let requests = provider.finish().await?;
    assert_eq!(requests.len(), 2);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 3);

    let log = NatsSessionLog::new_with_replicas(fixture.env.js.clone(), session.key.clone(), 1);
    let entries = log.load_events_async().await?;
    let entry = tool_results_entry(&entries)?;
    assert_entry_fits_broker(entry)?;
    let SessionLogEntry::ToolResults { results, .. } = entry else {
        unreachable!("matched ToolResults above")
    };
    // Each result passed the tool server's per-reply bound; only together do
    // they overflow the entry, so only the biggest one is degraded.
    assert_degraded_with_retained_original(
        &result_for(results, "call-large").output,
        &fixture.env.js,
    )
    .await?;
    assert_eq!(result_for(results, "call-a").output, sum_output(18_000));
    assert_eq!(result_for(results, "call-b").output, sum_output(18_000));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphan_repair_recovers_session_with_journaled_oversized_reply() -> Result<()> {
    require_nextest();
    let Some(server) = common::spawn_configured_nats_server("max_payload: 65536\n").await? else {
        return Ok(());
    };
    let root = tempfile::tempdir()?;
    let _env = [
        EnvGuard::set("HARNX_DATA_DIR", root.path().to_str().unwrap()),
        EnvGuard::set("HARNX_NATS_URL", server.url()),
        EnvGuard::set("HARNX_NATS_TOKEN", ""),
    ];
    let config = Arc::new(ConfigLock::new(local_nats_config(NatsServerSpec {
        name: "local",
        url: server.url(),
        token: None,
    })));
    let model_calls = Arc::new(AtomicUsize::new(0));
    let call_fn = counting_stub_call_fn(Arc::clone(&model_calls));
    let js = local_test_nats(server.url()).await?;
    let session_id = "entry3";
    let key = storage_key(session_id);
    let log = NatsSessionLog::new_with_replicas(js.clone(), key.clone(), 1);
    let (metadata_store, _) = seed_session_metadata(&js, session_id).await?;
    orphan::seed_journaled_reply(&js, &log, &key, session_id).await?;
    orphan::run_repair(config, &key, &metadata_store, call_fn).await?;
    orphan::assert_recovered(&js, &log, &model_calls).await
}
