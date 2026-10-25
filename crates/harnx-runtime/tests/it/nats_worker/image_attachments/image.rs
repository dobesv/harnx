use super::*;
use anyhow::Context;
use harnx_core::{
    attachments::{cid_for_data_url_with_session, read_attachment},
    cid_url::{CidUrl, SessionRef},
};
use harnx_runtime::config::SessionAttachmentPath;
use std::path::PathBuf;

const MIME: &str = "image/png; fixture=original";

pub(super) struct ImageAttachment {
    pub cache: PathBuf,
    cid: String,
    data_url: String,
    bytes: Vec<u8>,
}

impl ImageAttachment {
    pub fn new() -> Result<Self> {
        let session = SessionRef::new(None, "img123".into())?;
        let bytes = vec![42_u8; 40 * 1024];
        let data_url = format!(
            "data:{MIME};base64,{}",
            harnx_core::crypto::base64_encode(&bytes)
        );
        let cid = cid_for_data_url_with_session(&session, &data_url);
        let cache = Config::session_attachments_dir(SessionAttachmentPath {
            agent_name: harnx_core::agent_config::TEMP_AGENT_NAME,
            session_id: "img123",
        })
        .context("session cache directory")?;
        Ok(Self {
            cache,
            cid,
            data_url,
            bytes,
        })
    }

    pub fn script(&self) -> BridgeScript {
        BridgeScript {
            server_name: "images",
            tool_name: "large",
            result: json!({"content": [
                {"type": "text", "text": "sibling stays intact"},
                {"type": "image", "mimeType": MIME, "data": harnx_core::crypto::base64_encode(&self.bytes)},
                {"type": "image", "mimeType": "image/jpeg", "data": "aGVsbG8="},
                {"type": "resource_link", "name": "external", "uri": "https://invalid.invalid/never-fetch.png", "mimeType": "image/png"}
            ]}),
        }
    }

    pub fn assert_cache_missing(&self) {
        assert!(
            read_attachment(&self.cache, &self.cid).is_err(),
            "remote image not preseeded in worker cache"
        );
    }

    pub fn assert_cached(&self) -> Result<()> {
        assert_eq!(
            read_attachment(&self.cache, &self.cid)?,
            (self.bytes.clone(), MIME.into())
        );
        Ok(())
    }

    pub async fn assert_bridge_offload(
        &self,
        js: &async_nats::jetstream::Context,
        output: &Value,
    ) -> Result<()> {
        self.assert_cache_missing();
        assert_eq!(output["content"][0]["text"], "sibling stays intact");
        assert_eq!(output["content"][1]["type"], "resource_link");
        assert_eq!(output["content"][1]["uri"], self.cid);
        assert_eq!(
            harnx_runtime::nats_attachments::get_session_attachment(js, 1, &self.cid).await?,
            Some((self.bytes.clone(), MIME.into()))
        );
        Ok(())
    }

    pub async fn corrupt_remote(&self, js: &async_nats::jetstream::Context) -> Result<()> {
        let store = harnx_blob_store::media::ensure_attachments_bucket(js, 1).await?;
        let metadata = async_nats::jetstream::object_store::ObjectMetadata {
            name: CidUrl::parse(&self.cid)?.kv_key(),
            metadata: [("content_type".into(), MIME.into())].into_iter().collect(),
            ..Default::default()
        };
        store
            .put(metadata, &mut std::io::Cursor::new(b"corrupt remote image"))
            .await?;
        Ok(())
    }

    pub fn assert_provider_images(&self, requests: &[Value]) {
        assert_eq!(requests.len(), 4);
        assert!(request_images(&requests[0]).is_empty());
        for (round, request) in requests.iter().enumerate().skip(1) {
            let images = request_images(request);
            assert_eq!(
                images.len(),
                2,
                "round={round}, request={}",
                request.to_string().chars().take(2000).collect::<String>()
            );
            assert_eq!(images[0], self.data_url);
            assert_eq!(images[1], "data:image/jpeg;base64,aGVsbG8=");
            assert!(!request.to_string().contains("attachment unavailable"));
        }
    }
}

fn request_images(request: &Value) -> Vec<&str> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .filter_map(|part| part["image_url"]["url"].as_str())
        .collect()
}
