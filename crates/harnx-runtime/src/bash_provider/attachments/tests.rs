use std::collections::{HashMap, HashSet};

use super::*;
use crate::bash_provider::{BashEvalContextSlot, BashProvider};
use base64::Engine;
use harnx_blob_store::{
    media::{ensure_attachments_bucket, get_media, optional_attachments_bucket, put_media},
    media_cid_url,
};
use harnx_core::{
    abort::create_abort_signal,
    cid_url::SessionRef,
    hooks::{HookOutcome, HookResult, HookResultControl},
    instance::ServerScope,
    tool::{ToolError, ToolProvider},
};
use harnx_engine::tool::ToolEvalContext;
use harnx_script::FileSystem;
use serde_json::{json, Value};

const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+ip1sAAAAASUVORK5CYII=";

fn provider(fetcher: Option<Arc<dyn AttachmentFetcher>>) -> (BashProvider, Arc<ToolEvalContext>) {
    let slot = BashEvalContextSlot::new();
    let provider = BashProvider::new(crate::bash_provider::BashProviderConfig {
        active: true,
        inner_declarations: vec![],
        base_providers: vec![],
        slot: Some(slot.clone()),
        payload_limit: None,
        operator_inner_consent: false,
    })
    .with_attachment_fetcher(fetcher);
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
        confirm_tool_use_fn: Arc::new(|_, _, _| panic!("no tool calls expected")),
        dispatch_hook_fn: Arc::new(|_| {
            Box::pin(async {
                HookOutcome {
                    control: HookResultControl::Continue,
                    result: HookResult::default(),
                }
            })
        }),
        prepare_handoff_fn: None,
    });
    slot.set(&ctx);
    (provider, ctx)
}

async fn run(provider: &BashProvider, script: String) -> Value {
    match provider
        .call_tool("bash", json!({"script": script}), &create_abort_signal())
        .await
    {
        Ok(result) => result.value,
        Err(ToolError::Recoverable(error)) => harnx_core::partial_result::partial_result_of(&error)
            .unwrap_or_else(|| panic!("missing partial output: {error:#}"))
            .clone(),
        Err(ToolError::Fatal(error)) => panic!("unexpected fatal provider error: {error:#}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn nats_attachment_mount_reads_binary_is_readonly_and_resets_cache_per_run(
) -> anyhow::Result<()> {
    harnx_core::require_nextest();
    let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(());
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let store = ensure_attachments_bucket(&js, 1).await?;
    // Cross-session explicit references work just as attachment_read does. The
    // percent-encoded namespaced agent remains encoded in the VFS path.
    let url = media_cid_url(
        &SessionRef::new(Some("pantheon/atlas".into()), "shared1".into())?,
        HASH,
    );
    let png = base64::engine::general_purpose::STANDARD.decode(PNG_BASE64)?;
    put_media(&store, &url, &png, "image/png").await?;
    let (provider, _ctx) = provider(Some(fetcher(js)));
    let path = harnx_script::fs_attachments::attachment_path(&url.to_string())?;
    let result = run(
        &provider,
        format!(
            "ls /attachments; base64 -w0 '{}' | wc -c; base64 -w0 '{}'; echo",
            path.display(),
            path.display()
        ),
    )
    .await;
    assert_eq!(result["exit_code"], 0, "{result}");
    assert_eq!(
        result["stdout"],
        format!("{}\n{PNG_BASE64}\n", PNG_BASE64.len())
    );
    let result = run(&provider, format!("echo bad > '{}'", path.display())).await;
    assert_eq!(result["exit_code"], 1);
    assert!(
        result["stderr"].as_str().unwrap().contains("read-only"),
        "{result}"
    );
    assert_eq!(get_media(&store, &url).await?.unwrap().0, png);
    let check_exists = run(
        &provider,
        format!("[ -f '{}' ] && echo exists || echo missing", path.display()),
    )
    .await;
    assert_eq!(check_exists["exit_code"], 0);
    assert_eq!(check_exists["stdout"], "exists\n");
    store.delete(url.kv_key()).await?;
    let check_deleted = run(
        &provider,
        format!("[ -f '{}' ] && echo exists || echo missing", path.display()),
    )
    .await;
    assert_eq!(check_deleted["exit_code"], 0);
    assert_eq!(check_deleted["stdout"], "missing\n");
    let result = run(&provider, format!("cat '{}'", path.display())).await;
    assert_ne!(result["exit_code"], 0);
    assert!(
        result["stderr"]
            .as_str()
            .unwrap()
            .contains("attachment not found"),
        "{result}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn nats_fetcher_validates_cids_preserves_missing_errors_and_bounds_blob_download(
) -> anyhow::Result<()> {
    harnx_core::require_nextest();
    let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(());
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let fetcher = fetcher(js.clone());
    for cid in [
        "cid:media:agent/session/not-a-hash",
        "cid:plan:agent/session/plan",
        "cid:media:agent/../aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        assert_eq!(
            fetcher.fetch(cid, 100).await.unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
    let url = media_cid_url(
        &SessionRef::new(Some("agent".into()), "attach1".into())?,
        HASH,
    );
    assert_eq!(
        fetcher.fetch(&url.to_string(), 3).await.unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    // Metadata-only stat: missing bucket or object is Ok(None) per the trait
    // contract, and never provisions storage. Fetch still reports NotFound.
    assert_eq!(fetcher.size(&url.to_string()).await?, None);
    assert!(
        optional_attachments_bucket(&js).await?.is_none(),
        "reads must not provision storage"
    );
    let store = ensure_attachments_bucket(&js, 1).await?;
    put_media(&store, &url, &[0, 255, 1], "application/octet-stream").await?;
    assert_eq!(fetcher.size(&url.to_string()).await?, Some(3));
    assert!(fetcher
        .fetch(&url.to_string(), 2)
        .await
        .unwrap_err()
        .to_string()
        .contains("blob too large: 3 bytes exceeds limit of 2 bytes"));
    assert_eq!(fetcher.fetch(&url.to_string(), 3).await?, [0, 255, 1]);
    // Object deletion causes size() to return None without serving stale cached sizes.
    store.delete(url.kv_key()).await?;
    assert_eq!(fetcher.size(&url.to_string()).await?, None);
    // A missing object under an existing bucket is Ok(None) for size.
    let missing = media_cid_url(
        &SessionRef::new(Some("agent".into()), "attach2".into())?,
        HASH,
    );
    assert_eq!(fetcher.size(&missing.to_string()).await?, None);
    Ok(())
}

#[tokio::test]
async fn mount_is_empty_and_fails_clearly_without_a_broker() {
    let (provider, _ctx) = provider(None);
    let result = run(&provider, "ls /attachments".into()).await;
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["stdout"], "");
    let result = run(
        &provider,
        format!("cat /attachments/cid:media:agent/attach1/{HASH}"),
    )
    .await;
    assert!(
        result["stderr"]
            .as_str()
            .unwrap()
            .contains("attachment storage unavailable"),
        "{result}"
    );
}

#[tokio::test]
async fn python_heredoc_reads_tmp_and_real_nats_attachment() -> anyhow::Result<()> {
    harnx_core::require_nextest();
    let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(());
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let store = ensure_attachments_bucket(&js, 1).await?;
    let url = media_cid_url(
        &SessionRef::new(Some("pantheon/atlas".into()), "python1".into())?,
        HASH,
    );
    let png = base64::engine::general_purpose::STANDARD.decode(PNG_BASE64)?;
    put_media(&store, &url, &png, "image/png").await?;
    let (provider, _ctx) = provider(Some(fetcher(js)));
    let path = harnx_script::fs_attachments::attachment_path(&url.to_string())?;
    let result = run(&provider, format!(
        "echo temporary > /tmp/input.txt\npython3 - <<'EOF'\nfrom pathlib import Path\nprint(open('/tmp/input.txt').read(), end='')\ndata = Path('{}').read_bytes()\nprint(len(data))\nprint(list(data[:8]))\nEOF", path.display()
    )).await;
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(
        result["stdout"],
        format!(
            "temporary\n{}\n[137, 80, 78, 71, 13, 10, 26, 10]\n",
            png.len()
        )
    );
    assert!(result["trace"].as_array().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn nats_metadata_miss_sees_later_upload_and_deleted_blobs_are_missing() -> anyhow::Result<()>
{
    harnx_core::require_nextest();
    let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(());
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let store = ensure_attachments_bucket(&js, 1).await?;
    let url = media_cid_url(
        &SessionRef::new(Some("agent".into()), "late01".into())?,
        HASH,
    );
    let cid = url.to_string();
    let reader = fetcher(js.clone());
    let fs = AttachmentFs::new(reader.clone());
    let path = std::path::PathBuf::from(format!("/{cid}"));

    // Missing initially: stat/exists report missing, size is None.
    assert_eq!(reader.size(&cid).await?, None);
    assert!(!fs.exists(&path).await?);
    assert!(fs.stat(&path).await.is_err());

    // Newly uploaded: stat/exists see blob, size is Some(3).
    put_media(&store, &url, &[0, 255, 1], "application/octet-stream").await?;
    assert_eq!(reader.size(&cid).await?, Some(3));
    assert!(fs.exists(&path).await?);
    assert_eq!(fs.stat(&path).await?.size, 3);

    // Deleted: stat/exists and size immediately report missing (no stale memo).
    store.delete(url.kv_key()).await?;
    assert_eq!(reader.size(&cid).await?, None);
    assert!(!fs.exists(&path).await?);
    assert!(fs.stat(&path).await.is_err());

    // Second blob: missing initially then newly uploaded.
    let url2 = media_cid_url(
        &SessionRef::new(Some("agent".into()), "late02".into())?,
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    );
    let cid2 = url2.to_string();
    let path2 = std::path::PathBuf::from(format!("/{cid2}"));

    assert_eq!(reader.size(&cid2).await?, None);
    assert!(!fs.exists(&path2).await?);
    assert!(fs.stat(&path2).await.is_err());

    put_media(&store, &url2, &[42], "application/octet-stream").await?;
    assert_eq!(reader.size(&cid2).await?, Some(1));
    assert!(fs.exists(&path2).await?);
    assert_eq!(fs.stat(&path2).await?.size, 1);

    Ok(())
}
