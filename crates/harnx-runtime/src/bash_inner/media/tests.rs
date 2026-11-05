mod review;

use std::{
    collections::{HashMap, HashSet},
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use super::*;
use crate::bash_provider::{BashEvalContextSlot, BashProvider};
use harnx_core::{
    abort::{create_abort_signal, AbortSignal},
    attachments::cid_for_data_url_with_session,
    hooks::{HookOutcome, HookResult, HookResultControl},
    instance::ServerScope,
    partial_result::{error_with_partial_result, partial_result_of},
    tool::{ToolError, ToolProvider, ToolProviderOutput},
};
use harnx_engine::tool::ToolEvalContext;
use harnx_script::fs_attachments::AttachmentFetcher;

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+ip1sAAAAASUVORK5CYII=";

fn image() -> Value {
    json!({"type":"image", "mimeType":"image/png", "data":PNG})
}

fn owner() -> SessionRef {
    SessionRef::new(Some("pantheon/atlas".into()), "inner1".into()).unwrap()
}

#[derive(Default)]
struct MemoryStore(Mutex<HashMap<String, Vec<u8>>>);

#[async_trait::async_trait]
impl MediaStore for MemoryStore {
    async fn store(&self, data_url: &str) -> Result<ExternalizedAttachment> {
        let (mime_type, bytes) = crate::nats_attachments::parse_data_url(data_url)?;
        let cid = cid_for_data_url_with_session(&owner(), data_url);
        let size = bytes.len();
        self.0.lock().unwrap().insert(cid.clone(), bytes);
        Ok(ExternalizedAttachment {
            cid,
            mime_type,
            size,
        })
    }
}

#[async_trait::async_trait]
impl AttachmentFetcher for MemoryStore {
    async fn fetch(&self, cid: &str, max_bytes: u64) -> io::Result<Vec<u8>> {
        let bytes = self
            .0
            .lock()
            .unwrap()
            .get(cid)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "attachment missing"))?;
        assert!(bytes.len() as u64 <= max_bytes);
        Ok(bytes)
    }

    async fn size(&self, cid: &str) -> io::Result<Option<u64>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(cid)
            .map(|bytes| bytes.len() as u64))
    }
}

struct FakeTool {
    value: Value,
    fail: bool,
}

#[async_trait::async_trait]
impl ToolProvider for FakeTool {
    fn name(&self) -> &str {
        "inline-media"
    }
    fn has_tool(&self, name: &str) -> bool {
        name == "screenshot"
    }
    async fn call_tool(
        &self,
        _: &str,
        _: Value,
        _: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        if self.fail {
            Err(ToolError::Recoverable(error_with_partial_result(
                anyhow::anyhow!("screenshot failed"),
                self.value.clone(),
            )))
        } else {
            Ok(ToolProviderOutput::new(self.value.clone()))
        }
    }
}

fn harness(
    value: Value,
    store: Option<Arc<dyn MediaStore>>,
    fetcher: Option<Arc<dyn AttachmentFetcher>>,
    fail: bool,
) -> (BashProvider, Arc<ToolEvalContext>) {
    harness_with_post_hook(
        value,
        HarnessOptions {
            store,
            fetcher,
            fail,
            mutation: None,
        },
    )
}

struct HarnessOptions {
    store: Option<Arc<dyn MediaStore>>,
    fetcher: Option<Arc<dyn AttachmentFetcher>>,
    fail: bool,
    mutation: Option<Value>,
}

fn harness_with_post_hook(
    value: Value,
    options: HarnessOptions,
) -> (BashProvider, Arc<ToolEvalContext>) {
    let HarnessOptions {
        store,
        fetcher,
        fail,
        mutation,
    } = options;
    let slot = BashEvalContextSlot::new();
    let declaration = serde_json::from_value(json!({
        "name":"screenshot", "description":"Capture a screenshot.",
        "parameters":{"type":"object", "properties":{}},
    }))
    .unwrap();
    let provider = BashProvider::new(crate::bash_provider::BashProviderConfig {
        active: true,
        inner_declarations: vec![declaration],
        base_providers: vec![Arc::new(FakeTool { value, fail })],
        slot: Some(slot.clone()),
        payload_limit: None,
    })
    .with_attachment_fetcher(fetcher)
    .with_media_store(store);
    let ctx = Arc::new(ToolEvalContext {
        work_boundary: None,
        instance_id: ServerScope::new(),
        tool_round: Some(1),
        render: None,
        providers: vec![],
        allowed_tool_names: HashSet::new(),
        current_agent_package: None,
        handoff_targets: HashMap::new(),
        emit_tool_call_fn: Arc::new(|_, _| {}),
        emit_tool_result_fn: Arc::new(|_, _| {}),
        emit_tool_blocked_fn: Arc::new(|_, _| {}),
        emit_tool_update_fn: Arc::new(|_, _| {}),
        confirm_tool_use_fn: Arc::new(|_, _, _| panic!("no confirmation expected")),
        dispatch_hook_fn: Arc::new(move |event| {
            let mutated_tool_response =
                if matches!(event, harnx_core::hooks::HookEvent::PostToolUse { .. }) {
                    mutation.clone()
                } else {
                    None
                };
            Box::pin(async move {
                HookOutcome {
                    control: HookResultControl::Continue,
                    result: HookResult {
                        mutated_tool_response,
                        ..Default::default()
                    },
                }
            })
        }),
        prepare_handoff_fn: None,
    });
    slot.set(&ctx);
    (provider, ctx)
}

async fn run(provider: &BashProvider, script: &str) -> Value {
    let result = provider
        .call_tool("bash", json!({"script": script}), &create_abort_signal())
        .await;
    let value = match result {
        Ok(result) => result.value,
        Err(ToolError::Recoverable(error)) => partial_result_of(&error).unwrap().clone(),
        Err(ToolError::Fatal(error)) => panic!("unexpected fatal provider error: {error:#}"),
    };
    assert_no_inline_png(&value);
    value
}

#[track_caller]
fn assert_exit_code(result: &Value, expected: i32) {
    assert_eq!(result["exit_code"], expected, "{result}");
}

#[track_caller]
fn assert_no_inline_png(value: &Value) {
    assert!(
        !value.to_string().contains(PNG),
        "inline PNG leaked: {value}"
    );
}

#[track_caller]
fn assert_single_upload(store: &MemoryStore) {
    assert_eq!(store.0.lock().unwrap().len(), 1);
}

#[track_caller]
fn assert_command_not_run(result: &Value) {
    assert!(!result["stdout"]
        .as_str()
        .unwrap()
        .contains("should-not-run"));
}

#[tokio::test]
async fn inline_png_is_readable_before_command_returns_in_all_output_modes() {
    for option in ["", "-o text", "-o json"] {
        let store = Arc::new(MemoryStore::default());
        let (provider, _ctx) = harness(
            json!({"content":[image()]}),
            Some(store.clone()),
            Some(store.clone()),
            false,
        );
        let path = if option == "-o json" {
            "screenshot -o json | jq -r '.content[0].path'"
        } else {
            "screenshot | sed 's/^.*path=//'"
        };
        let script = format!(
            "path=$({path}); test -n \"$path\"; base64 -w0 \"$path\" > /tmp/actual; test \"$(cat /tmp/actual)\" = '{PNG}'; wc -c < \"$path\"; screenshot {option}"
        );
        let result = run(&provider, &script).await;
        assert_exit_code(&result, 0);
        assert!(
            result["stdout"].as_str().unwrap().starts_with("68\n"),
            "{result}"
        );
        assert!(result["stdout"]
            .as_str()
            .unwrap()
            .contains("[attachment: cid:media:pantheon%2Fatlas/inner1/"));
        assert_single_upload(&store);
    }
}

#[tokio::test]
async fn structured_media_and_serialized_json_mirrors_are_externalized() {
    let store = Arc::new(MemoryStore::default());
    let structured = json!({"nested":[image(), format!("data:image/png;base64,{PNG}")]});
    let mut value = json!({"structuredContent":structured, "content":[{"type":"text", "text":structured.to_string()}]});
    InnerMedia::new(Some(store.clone()))
        .prepare(&mut value)
        .await
        .unwrap();
    assert_no_inline_png(&value);
    assert!(value["structuredContent"]["nested"][0]["path"]
        .as_str()
        .unwrap()
        .starts_with("/attachments/cid:media:"));
    let mirror: Value =
        serde_json::from_str(value["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(mirror, value["structuredContent"]);
    assert_single_upload(&store);
    let (provider, _ctx) = harness(
        json!({"content":[], "structuredContent": {"image":image()}}),
        Some(store.clone()),
        Some(store),
        false,
    );
    let result = run(&provider, "screenshot -o json | jq -r '.image.path'").await;
    assert_exit_code(&result, 0);
    assert!(result["stdout"]
        .as_str()
        .unwrap()
        .starts_with("/attachments/cid:media:"));
}

#[tokio::test]
async fn embedded_binary_text_resources_and_audio_use_the_same_store() {
    let store = Arc::new(MemoryStore::default());
    let mut value = json!({"content":[
        {"type":"resource", "resource":{"uri":"file:///image.png", "mimeType":"image/png", "blob":PNG}},
        {"type":"resource", "resource":{"uri":"file:///note.txt", "mimeType":"text/plain", "text":"hello"}},
        {"type":"audio", "mimeType":"audio/wav", "data":"AP8B"}
    ]});
    InnerMedia::new(Some(store.clone()))
        .prepare(&mut value)
        .await
        .unwrap();
    for (i, expected) in [
        harnx_core::crypto::base64_decode(PNG).unwrap(),
        b"hello".to_vec(),
        vec![0, 255, 1],
    ]
    .iter()
    .enumerate()
    {
        let uri = value["content"][i]["uri"].as_str().unwrap();
        assert_eq!(&store.fetch(uri, 1000).await.unwrap(), expected);
        assert!(value["content"][i]["text"]
            .as_str()
            .unwrap()
            .contains("path=/attachments/"));
    }
    assert_no_inline_png(&value);
}

#[tokio::test]
async fn malformed_media_fails_without_echoing_inline_payloads() {
    for block in [
        json!({"type":"image", "data":PNG}),
        json!({"type":"image", "mimeType":"image/png", "data":"SECRET-INVALID-BASE64"}),
        json!({"type":"resource", "resource":{"blob":4}}),
        json!({"type":"resource_link", "uri":"cid:media:bad", "mimeType":"image/png"}),
    ] {
        let (provider, _ctx) = harness(
            json!({"content":[block]}),
            Some(Arc::new(MemoryStore::default())),
            None,
            false,
        );
        let result = run(&provider, "screenshot -o json; echo should-not-run").await;
        assert_exit_code(&result, 1);
        assert!(!result.to_string().contains("SECRET"));
        assert_command_not_run(&result);
        assert_eq!(result["trace"][0]["status"], "tool_error");
    }
}

#[tokio::test]
async fn recoverable_partial_results_have_no_inline_media() {
    let store = Arc::new(MemoryStore::default());
    let (provider, _ctx) = harness(
        json!({"content":[image()]}),
        Some(store.clone()),
        Some(store.clone()),
        true,
    );
    let result = run(&provider, "screenshot -o json").await;
    assert_exit_code(&result, 1);
    assert!(result["stdout"]
        .as_str()
        .unwrap()
        .contains("path=/attachments/"));
    assert_single_upload(&store);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_inline_png_round_trip_has_correct_owner_and_immediate_read_consistency() -> Result<()>
{
    harnx_core::require_nextest();
    let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(());
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let store = nats_store(js.clone(), Some(owner()), 1);
    let fetcher = crate::bash_provider::attachments::fetcher(js.clone());
    let (provider, _ctx) = harness(
        json!({"content":[image()]}),
        Some(store),
        Some(fetcher),
        false,
    );
    let result = run(&provider, &format!(
        "path=$(screenshot -o json | jq -r '.content[0].path'); base64 -w0 \"$path\" > /tmp/image; test \"$(cat /tmp/image)\" = '{PNG}'; echo \"$path\"; wc -c < \"$path\""
    )).await;
    assert_exit_code(&result, 0);
    let expected_cid =
        cid_for_data_url_with_session(&owner(), &format!("data:image/png;base64,{PNG}"));
    assert_eq!(
        result["stdout"],
        format!("/attachments/{expected_cid}\n68\n")
    );
    let object_store = harnx_blob_store::media::optional_attachments_bucket(&js)
        .await?
        .unwrap();
    let (bytes, mime) =
        harnx_blob_store::media::get_media(&object_store, &CidUrl::parse(&expected_cid)?)
            .await?
            .unwrap();
    assert_eq!(bytes, harnx_core::crypto::base64_decode(PNG)?);
    assert_eq!(mime, "image/png");
    Ok(())
}

#[tokio::test]
async fn absent_storage_stops_even_with_errexit_disabled_and_plain_results_still_work() {
    let (provider, _ctx) = harness(json!({"content":[image()]}), None, None, false);
    let result = run(&provider, "set +e; screenshot -o json; echo should-not-run").await;
    assert_ne!(result["exit_code"], 0, "{result}");
    assert!(
        result["stderr"]
            .as_str()
            .unwrap()
            .contains("attachment storage unavailable"),
        "{result}"
    );
    assert_command_not_run(&result);
    assert_eq!(result["trace"][0]["status"], "fatal");
    for plain in [
        Value::Null,
        json!({"answer":42}),
        json!({"content":[{"type":"text","text":"no media"}]}),
    ] {
        let mut prepared = plain.clone();
        InnerMedia::new(None).prepare(&mut prepared).await.unwrap();
        assert_eq!(prepared, plain);
    }
}

struct GatedStore {
    memory: MemoryStore,
    started: tokio::sync::mpsc::UnboundedSender<()>,
    release: tokio::sync::Semaphore,
    active: std::sync::atomic::AtomicUsize,
    peak: std::sync::atomic::AtomicUsize,
}

struct Active<'a>(&'a std::sync::atomic::AtomicUsize);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl MediaStore for GatedStore {
    async fn store(&self, data_url: &str) -> Result<ExternalizedAttachment> {
        use std::sync::atomic::Ordering::SeqCst;
        let active = self.active.fetch_add(1, SeqCst) + 1;
        let _active = Active(&self.active);
        self.peak.fetch_max(active, SeqCst);
        self.started.send(())?;
        self.release.acquire().await?.forget();
        self.memory.store(data_url).await
    }
}

fn gated_store() -> (Arc<GatedStore>, tokio::sync::mpsc::UnboundedReceiver<()>) {
    let (started, rx) = tokio::sync::mpsc::unbounded_channel();
    (
        Arc::new(GatedStore {
            memory: MemoryStore::default(),
            started,
            release: tokio::sync::Semaphore::new(0),
            active: std::sync::atomic::AtomicUsize::new(0),
            peak: std::sync::atomic::AtomicUsize::new(0),
        }),
        rx,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_media_writes_cancel_without_publishing_a_path_or_detached_upload() {
    let (store, mut started) = gated_store();
    let (provider, _ctx) = harness(
        json!({"content":[image()]}),
        Some(store.clone()),
        None,
        false,
    );
    let abort = create_abort_signal();
    let task_abort = abort.clone();
    let task = tokio::spawn(async move {
        provider
            .call_tool(
                "bash",
                json!({"script":"screenshot; echo should-not-run"}),
                &task_abort,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), started.recv())
        .await
        .unwrap()
        .unwrap();
    abort.set_ctrlc();
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    let Err(ToolError::Fatal(error)) = result else {
        panic!("expected fatal abort");
    };
    let report = partial_result_of(&error).unwrap();
    assert_eq!(report["status"], "cancelled");
    assert_eq!(report["stdout"], "");
    assert_eq!(report["trace"][0]["status"], "cancelled");
    assert_no_inline_png(report);
    assert_eq!(store.active.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(store.memory.0.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrency_permits_cover_media_writes_until_the_command_finishes() {
    let (store, mut started) = gated_store();
    let (provider, _ctx) = harness(
        json!({"content":[image()]}),
        Some(store.clone()),
        None,
        false,
    );
    let task = tokio::spawn(async move {
        run(
            &provider,
            "for i in 1 2 3 4 5 6 7 8 9 10 11 12; do screenshot >/tmp/$i & done; wait; echo done",
        )
        .await
    });
    for _ in 0..super::super::DEFAULT_MAX_CONCURRENT_INNER_CALLS {
        tokio::time::timeout(Duration::from_secs(5), started.recv())
            .await
            .unwrap()
            .unwrap();
    }
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(
        started.try_recv().is_err(),
        "media writes exceeded the inner-call cap"
    );
    store.release.add_permits(12);
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert_exit_code(&result, 0);
    assert_eq!(result["stdout"], "done\n");
    assert_eq!(result["trace"].as_array().unwrap().len(), 12);
    assert!(store.peak.load(std::sync::atomic::Ordering::SeqCst) <= 8);
}

#[tokio::test]
async fn oversized_inline_media_and_excessive_nesting_fail_without_storage() {
    let max = harnx_script::FsLimits::default().max_file_size;
    let oversized = format!(
        "data:image/png;base64,{}",
        "A".repeat((max.div_ceil(3) * 4 + 4) as usize)
    );
    let error = InnerMedia::new(None)
        .prepare(&mut json!({"url":oversized}))
        .await
        .unwrap_err();
    assert!(error.downcast_ref::<InvalidMedia>().is_some());
    assert!(error.to_string().contains("file-size limit"));
    let mut nested = image();
    for _ in 0..128 {
        nested = json!({"nested":nested});
    }
    let error = InnerMedia::new(None)
        .prepare(&mut nested)
        .await
        .unwrap_err();
    assert!(error.downcast_ref::<InvalidMedia>().is_some());
    assert!(error.to_string().contains("nesting limit"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_duplicate_inline_uploads_are_readable_while_other_commands_are_writing() -> Result<()>
{
    harnx_core::require_nextest();
    let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(());
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    for parent in [
        owner(),
        SessionRef::new(Some("other/agent".into()), "inner2".into())?,
        SessionRef::new(None, "inner3".into())?,
    ] {
        let store = nats_store(js.clone(), Some(parent.clone()), 1);
        let fetcher = crate::bash_provider::attachments::fetcher(js.clone());
        let (provider, _ctx) = harness(
            json!({"content":[image()]}),
            Some(store),
            Some(fetcher),
            false,
        );
        let result = run(&provider, &format!(
            "for i in 1 2 3 4 5 6 7 8 9 10 11 12; do {{ path=$(screenshot -o json | jq -r '.content[0].path'); test -n \"$path\"; test \"$(base64 -w0 \"$path\")\" = '{PNG}'; echo verified; }} > /tmp/$i & done; wait; cat /tmp/*; echo done"
        )).await;
        assert_exit_code(&result, 0);
        assert_eq!(
            result["stdout"],
            format!("{}done\n", "verified\n".repeat(12)),
            "{result}"
        );
        assert_eq!(result["trace"].as_array().unwrap().len(), 12);
        assert!(result["trace"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["status"] == "ok"));
        let cid = cid_for_data_url_with_session(&parent, &format!("data:image/png;base64,{PNG}"));
        let store = harnx_blob_store::media::optional_attachments_bucket(&js)
            .await?
            .unwrap();
        assert!(
            harnx_blob_store::media::get_media(&store, &CidUrl::parse(&cid)?)
                .await?
                .is_some()
        );
    }
    let no_owner = nats_store(js, None, 1);
    let error = no_owner
        .store(&format!("data:image/png;base64,{PNG}"))
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("valid agent/session identity"));
    Ok(())
}

#[tokio::test]
async fn post_hook_injected_images_are_externalized_before_script_rendering() {
    let store = Arc::new(MemoryStore::default());
    let (provider, _ctx) = harness_with_post_hook(
        json!({"content":[{"type":"text", "text":"original"}]}),
        HarnessOptions {
            store: Some(store.clone()),
            fetcher: Some(store),
            fail: false,
            mutation: Some(json!({"content":[image()]})),
        },
    );
    let result = run(&provider, &format!(
        "path=$(screenshot -o json | jq -r '.content[0].path'); test \"$(base64 -w0 \"$path\")\" = '{PNG}'; echo verified"
    )).await;
    assert_exit_code(&result, 0);
    assert_eq!(result["stdout"], "verified\n");
}

#[tokio::test]
async fn data_uri_links_are_externalized_and_remote_links_never_trigger_storage() {
    let store = Arc::new(MemoryStore::default());
    let processor = InnerMedia::new(Some(store.clone()));
    let mut value = json!({"content":[
        {"type":"resource_link", "uri":format!("data:image/png;base64,{PNG}"), "mimeType":"image/png"},
        {"type":"resource", "resource":{"uri":format!("data:image/png;base64,{PNG}")}},
        {"type":"resource_link", "uri":"https://example.com/image.png", "mimeType":"image/png"}
    ]});
    processor.prepare(&mut value).await.unwrap();
    assert_no_inline_png(&value);
    assert!(value["content"][0]["path"].is_string());
    assert_eq!(value["content"][0], value["content"][1]);
    assert_eq!(value["content"][2]["uri"], "https://example.com/image.png");
    assert_single_upload(&store);
    let prepared = value.clone();
    processor.prepare(&mut value).await.unwrap();
    assert_eq!(value, prepared, "already externalized results are stable");
}

#[tokio::test]
async fn structured_unwrapped_media_bodies_are_replaced_without_guessing_business_data() {
    let store = Arc::new(MemoryStore::default());
    let (provider, _ctx) = harness(
        json!({"structuredContent": {
            "image": {"mimeType":"IMAGE/PNG", "data":PNG},
            "resource": {"mimeType":"application/octet-stream", "blob":PNG},
            "ordinary": {"mimeType":"application/json", "data":"business value"}
        }}),
        Some(store.clone()),
        Some(store),
        false,
    );
    let result = run(&provider, "screenshot -o json").await;
    assert_exit_code(&result, 0);
    let document: Value = serde_json::from_str(result["stdout"].as_str().unwrap()).unwrap();
    assert!(document["image"]["path"].is_string());
    assert!(document["resource"]["path"].is_string());
    assert_eq!(document["ordinary"]["data"], "business value");
}
