use super::*;

pub(super) struct MixedParts {
    pub original: Value,
    large: String,
    bytes: Vec<u8>,
    annotation: Value,
}

impl MixedParts {
    pub fn new() -> Self {
        let large = "head\"\n日本語👋\\\u{0000}".repeat(4000) + "END";
        let bytes = [0, 1, 254, 255].repeat(12000);
        let annotation = json!({"audience": ["assistant"], "priority": 0.75});
        let original = json!({"content": [
            {"type": "text", "text": "small\r\nunchanged\u{0000}", "annotations": annotation},
            {"type": "text", "text": large, "annotations": annotation, "_meta": {"part": 1}},
            {"type": "image", "data": STANDARD.encode(&bytes), "mimeType": "image/png", "annotations": annotation, "_meta": {"part": 2}},
            {"type": "audio", "data": STANDARD.encode(&bytes), "mimeType": "audio/wav; codecs=pcm"},
            {"type": "resource", "resource": {"uri": "file:///large.json", "text": large, "mimeType": "application/json; charset=utf-8", "_meta": {"nested": "retained"}}, "annotations": annotation},
            {"type": "resource", "resource": {"uri": "file:///blob", "blob": STANDARD.encode(&bytes), "mimeType": "application/octet-stream"}},
            {"type": "resource_link", "uri": "file:///unchanged", "name": "original"},
            {"type": "image", "data": "AA==", "mimeType": "image/png"},
            {"type": "resource", "resource": {"uri": "file:///small", "text": "untouched", "mimeType": "text/plain"}}
        ], "structuredContent": {"unchanged": true}, "_meta": {"unchanged": [1, 2]}, "isError": true});
        Self {
            original,
            large,
            bytes,
            annotation,
        }
    }

    pub fn assert_fields_preserved(&self, result: &Value) {
        for field in ["structuredContent", "_meta", "isError"] {
            assert_eq!(result[field], self.original[field]);
        }
        for index in [0, 6, 7, 8] {
            assert_eq!(result["content"][index], self.original["content"][index]);
        }
        for index in [1, 2, 4] {
            assert_eq!(result["content"][index]["annotations"], self.annotation);
        }
        for index in [1, 2] {
            assert_eq!(
                result["content"][index]["_meta"],
                self.original["content"][index]["_meta"]
            );
        }
        assert_eq!(
            result["content"][4]["resource"]["_meta"],
            self.original["content"][4]["resource"]["_meta"]
        );
        assert_eq!(
            result["content"][5]["description"],
            "Original resource: file:///blob"
        );
    }

    pub async fn assert_binary_originals(
        &self,
        js: &async_nats::jetstream::Context,
        result: &Value,
    ) -> Result<()> {
        for (index, mime) in [
            (2, "image/png"),
            (3, "audio/wav; codecs=pcm"),
            (5, "application/octet-stream"),
        ] {
            let part = &result["content"][index];
            assert_eq!(part["type"], "resource_link");
            assert_eq!(part["uri"], cid(mime, &self.bytes));
            assert_eq!(part["mimeType"], mime);
            assert_eq!(part["size"], self.bytes.len());
            assert_eq!(
                stored(js, part["uri"].as_str().unwrap()).await?,
                (self.bytes.clone(), mime.into())
            );
            assert!(part.get("data").is_none());
        }
        Ok(())
    }

    pub async fn assert_text_originals(
        &self,
        js: &async_nats::jetstream::Context,
        result: &Value,
    ) -> Result<()> {
        for (index, mime) in [(1, "text/plain"), (4, "application/json; charset=utf-8")] {
            let uri = cid(mime, self.large.as_bytes());
            assert_eq!(
                stored(js, &uri).await?,
                (self.large.as_bytes().to_vec(), mime.into())
            );
            let preview = if index == 1 {
                &result["content"][index]["text"]
            } else {
                &result["content"][index]["resource"]["text"]
            };
            let preview = preview.as_str().unwrap();
            assert!(preview.contains(&uri));
            assert!(preview.contains("byte_offset/byte_limit"));
            assert!(preview.contains("head"));
            assert!(preview.contains("END"));
            assert!(!preview.contains('\u{fffd}'));
        }
        // Same bytes with different MIME must not alias one stored MIME record.
        assert_ne!(
            cid("text/plain", self.large.as_bytes()),
            cid("application/json; charset=utf-8", self.large.as_bytes())
        );
        Ok(())
    }
}
