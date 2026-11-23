use super::image::request_images;
use super::*;
use anyhow::Context;
use harnx_core::attachments::{cid_for_data_url_with_session, read_attachment};
use harnx_core::cid_url::SessionRef;
use harnx_core::crypto::{base64_decode, base64_encode};
use harnx_runtime::config::SessionAttachmentPath;
use harnx_runtime::nats_attachments::get_session_attachment;
use harnx_toolset::ToolReply;
use std::path::Path;
use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

#[path = "native/wire.rs"]
mod wire;
use wire::{NativeToolset, ObservedCall, WireTap};

const SESSION_ID: &str = "ntv123";
const SMALL: &str = "small";
const LARGE: &str = "large";
const PNG: &str = "image/png";
const SMALL_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGPQ1NIGAAD+AH+Ckjc8AAAAAElFTkSuQmCC";

pub(super) enum Source {
    Fetch(MockServer),
    Fs(tempfile::TempDir),
}

impl Source {
    pub async fn fetch() -> Self {
        Self::Fetch(MockServer::start().await)
    }

    pub fn fs() -> Result<Self> {
        Ok(Self::Fs(tempfile::tempdir()?))
    }

    async fn add_image(&self, name: &str, bytes: &[u8]) -> Result<Value> {
        let path = format!("{name}.png");
        match self {
            Self::Fetch(http) => {
                Mock::given(matchers::method("GET"))
                    .and(matchers::path(format!("/{path}")))
                    .respond_with(ResponseTemplate::new(200).set_body_raw(bytes, PNG))
                    .mount(http)
                    .await;
                Ok(json!({"url": format!("{}/{path}", http.uri())}))
            }
            Self::Fs(dir) => {
                let path = dir.path().join(path);
                std::fs::write(&path, bytes)?;
                Ok(json!({"path": path}))
            }
        }
    }

    fn invalidate_fs_source(&self) -> Result<()> {
        // If the restarted fs tool executes the read again, it can't produce
        // the original result. Fetch has an independent HTTP request count.
        if let Self::Fs(dir) = self {
            std::fs::remove_file(dir.path().join("large.png"))?;
        }
        Ok(())
    }

    async fn assert_fetch_count(&self) -> Result<()> {
        if let Self::Fetch(http) = self {
            let requests = http.received_requests().await.context("HTTP request log")?;
            let mut paths: Vec<_> = requests.iter().map(|r| r.url.path()).collect();
            paths.sort_unstable();
            assert_eq!(paths, ["/large.png", "/small.png"], "fetch re-executed");
        }
        Ok(())
    }
}

pub(super) async fn exercise(source: Source) -> Result<()> {
    require_nextest();
    let env = ResultEnvironment::start().await?;
    let toolset = NativeToolset::new(&env, &source);
    let mut child = toolset.start().await?;
    let wire = WireTap::subscribe(&toolset).await?;
    let images = Images::new()?;
    assert!(images.large.len() > toolset.client().max_payload());
    let provider = images.provider(&source, toolset.exposed_tool).await?;
    let session = ProviderSession::create(
        &env,
        SessionSpec {
            id: SESSION_ID,
            api_base: &provider.base,
            supports_vision: true,
        },
    )
    .await?;

    session.run_turn(None).await?;
    let [small, large] = wire.calls().await?;
    // Parallel tool calls need not complete (or be published) in provider order.
    let (small, large) = match small.request.tool_call_id.as_deref() {
        Some(SMALL) => (small, large),
        _ => (large, small),
    };
    assert_eq!(small.request.tool_call_id.as_deref(), Some(SMALL));
    assert_eq!(large.request.tool_call_id.as_deref(), Some(LARGE));
    images.assert_wire_replies(&small, &large)?;
    images.assert_cache(&env).await?;

    child.kill().await?;
    source.invalidate_fs_source()?;
    images.remove_remote(&env).await?;
    assert_journal_replay(&toolset, &large).await?;
    images.assert_remote_missing(&env).await?;

    // Reload durable messages with the warmed cache and no running toolset or
    // remote blob. The next provider request must contain the same image bytes.
    session.replay().await?;
    images.assert_provider_requests(&provider.finish().await?)?;
    source.assert_fetch_count().await
}

async fn assert_journal_replay(toolset: &NativeToolset<'_>, call: &ObservedCall) -> Result<()> {
    let mut restarted = toolset.start().await?;
    let replay = toolset.redeliver(call).await?;
    assert_eq!(replay.payload, call.reply, "replayed wire reply changed");
    restarted.kill().await?;
    Ok(())
}

struct Images {
    small: Vec<u8>,
    large: Vec<u8>,
    owner: SessionRef,
    cid: String,
}

impl Images {
    fn new() -> Result<Self> {
        let small = base64_decode(SMALL_PNG)?;
        let large = large_png(&small);
        let owner = SessionRef::new(None, SESSION_ID.into())?;
        let cid = cid_for_data_url_with_session(
            &owner,
            &format!("data:{PNG};base64,{}", base64_encode(&large)),
        );
        Ok(Self {
            small,
            large,
            owner,
            cid,
        })
    }

    async fn provider(&self, source: &Source, tool: &'static str) -> Result<CapturedProvider> {
        let small_args = source.add_image(SMALL, &self.small).await?;
        let large_args = source.add_image(LARGE, &self.large).await?;
        CapturedProvider::start_with_arguments(
            vec![
                (tool, SMALL, small_args.to_string()),
                (tool, LARGE, large_args.to_string()),
            ],
            "images received",
        )
        .await
    }

    fn assert_wire_replies(&self, small: &ObservedCall, large: &ObservedCall) -> Result<()> {
        let inline = image_block(small, "image")?;
        assert_eq!(inline["data"], SMALL_PNG);
        assert!(inline.get("uri").is_none());
        let link = image_block(large, "resource_link")?;
        assert_eq!(link["uri"], self.cid);
        assert_eq!(link["size"], self.large.len());
        assert!(link.get("data").is_none());
        Ok(())
    }

    async fn assert_cache(&self, env: &ResultEnvironment) -> Result<()> {
        let cache = Config::session_attachments_dir(SessionAttachmentPath {
            agent_name: harnx_core::agent_config::TEMP_AGENT_NAME,
            session_id: SESSION_ID,
        })
        .context("native image cache directory")?;
        let cached = read_attachment(Path::new(&cache), &self.cid)?;
        self.assert_large_png(cached);
        let remote = get_session_attachment(&env.js, 1, &self.cid)
            .await?
            .context("offloaded PNG")?;
        self.assert_large_png(remote);
        Ok(())
    }

    fn assert_large_png(&self, (bytes, mime): (Vec<u8>, String)) {
        assert_eq!(mime, PNG);
        assert!(bytes == self.large, "offloaded PNG bytes changed");
    }

    async fn remove_remote(&self, env: &ResultEnvironment) -> Result<()> {
        // Purge rather than leave object metadata that could hide a duplicate
        // upload. The worker can separately store the small inline result when
        // it persists the transcript, so don't assume only one object exists.
        let deleted =
            harnx_blob_store::media::delete_media_prefix(&env.js, &self.owner.owner()).await?;
        assert!(deleted > 0, "no remote images to purge");
        Ok(())
    }

    async fn assert_remote_missing(&self, env: &ResultEnvironment) -> Result<()> {
        assert!(
            get_session_attachment(&env.js, 1, &self.cid)
                .await?
                .is_none(),
            "journal replay re-uploaded the PNG"
        );
        Ok(())
    }

    fn assert_provider_requests(&self, requests: &[Value]) -> Result<()> {
        assert_eq!(
            requests.len(),
            3,
            "initial request, same-turn result, next turn"
        );
        assert!(request_images(&requests[0]).is_empty());
        for (index, request) in requests.iter().enumerate().skip(1) {
            let mut urls = request_images(request);
            urls.sort_unstable_by_key(|url| url.len());
            assert_eq!(urls.len(), 2, "provider request {index} lost an image");
            for (url, expected) in urls.into_iter().zip([&self.small, &self.large]) {
                let data = url
                    .strip_prefix("data:image/png;base64,")
                    .context("PNG data URL")?;
                assert!(
                    base64_decode(data)? == *expected,
                    "PNG bytes changed in provider request {index}"
                );
            }
        }
        Ok(())
    }
}

fn image_block(call: &ObservedCall, kind: &str) -> Result<Value> {
    let reply: ToolReply = serde_json::from_slice(&call.reply)?;
    assert_eq!(reply.call_id, call.request.call_id);
    let result = reply
        .result
        .map_err(|e| anyhow::anyhow!("native tool failed: {e:?}"))?;
    assert_ne!(result["isError"], true);
    let content = result["content"].as_array().context("native MCP content")?;
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], kind);
    assert_eq!(content[0]["mimeType"], PNG);
    assert_eq!(content[1]["type"], "text");
    Ok(content[0].clone())
}

fn large_png(small: &[u8]) -> Vec<u8> {
    // Insert a valid private ancillary chunk before IEND. Keep the fixture a
    // PNG while exercising the raw-byte limit, with no big binary in git.
    const PAYLOAD_LEN: u32 = 1280 * 1024;
    let (image, iend) = small.split_at(small.len() - 12);
    let mut large = image.to_vec();
    large.extend(PAYLOAD_LEN.to_be_bytes());
    let crc_start = large.len();
    large.extend(b"ruSt");
    large.extend((0..PAYLOAD_LEN).map(|i| (i ^ (i >> 8)) as u8));
    large.extend(crc32fast::hash(&large[crc_start..]).to_be_bytes());
    large.extend(iend);
    large
}
