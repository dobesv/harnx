//! The fields of a fetch tool's script-style result, for `help <tool>`.

use harnx_toolset::{output_field, output_nullable_field};
use serde_json::{json, Value};

pub(crate) fn page_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "text": output_field("string", "the page text"),
            "notes": {
                "type": "array",
                "items": { "type": "string" },
                "description": "what was cut off and how to continue",
            },
            "url": output_field("string", "the url that was requested"),
            "final_url": output_field("string", "the url after redirects"),
            "content_type": output_nullable_field("string", "the response content type"),
        }
    })
}
