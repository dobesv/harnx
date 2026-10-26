use super::{first_text, media_url, put_media, TestContext, HASH_A};
use harnx_core::cid_url::CidUrl;
use serde_json::{json, Value};

pub struct StoredAttachment {
    ctx: TestContext,
    url: CidUrl,
    byte_count: usize,
}

impl StoredAttachment {
    pub async fn start(bytes: &[u8], mime: &str) -> Option<Self> {
        let ctx = TestContext::start().await?;
        let url = media_url(HASH_A);
        put_media(&ctx.store, &url, bytes, mime)
            .await
            .expect("seed attachment object store");
        Some(Self {
            ctx,
            url,
            byte_count: bytes.len(),
        })
    }

    pub async fn read(&self, mut selectors: Value) -> Value {
        selectors["url"] = json!(self.url.to_string());
        self.ctx.invoke("attachment_read", selectors, None).await
    }

    pub async fn page(&self, offset: usize, limit: usize) -> Value {
        self.read(json!({ "byte_offset": offset, "byte_limit": limit }))
            .await
    }

    pub async fn reconstruct(&self, limit: usize) -> String {
        let mut text = String::new();
        let mut offset = 0;
        for _ in 0..=self.byte_count {
            let result = self.page(offset, limit).await;
            assert_eq!(result["isError"], false, "{result}");
            let body = first_text(&result);
            assert!(body.len() <= limit.max(4), "page exceeds UTF-8 allowance");
            text.push_str(body);
            let next = next_offset(&result);
            assert_page(&result, body, next);
            let Some(next) = next else {
                assert_eq!(text.len(), self.byte_count);
                return text;
            };
            assert_eq!(next, offset + body.len(), "pages must be contiguous");
            assert!(next > offset, "pagination must advance");
            offset = next;
        }
        panic!("pagination did not finish");
    }
}

pub fn metadata(result: &Value) -> &str {
    result["content"][1]["text"]
        .as_str()
        .expect("page metadata")
}

fn next_offset(result: &Value) -> Option<usize> {
    let summary = metadata(result);
    if summary.contains("end_of_content: true") {
        return None;
    }
    Some(
        summary
            .split("next_byte_offset: ")
            .nth(1)
            .expect("next offset or end marker")
            .trim()
            .parse()
            .expect("numeric next offset"),
    )
}

pub fn assert_page(result: &Value, text: &str, next: Option<usize>) {
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(first_text(result), text);
    for (index, audience) in [(0, "assistant"), (1, "user")] {
        assert_eq!(
            result["content"][index]["annotations"]["audience"],
            json!([audience])
        );
    }
    assert_eq!(next_offset(result), next);
}
