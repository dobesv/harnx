use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SearchResponse {
    pub facets: Facets,
    pub hits: Hits,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Facets {
    pub count: u64,
    pub lang: BucketFacet,
    pub repo: BucketFacet,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct BucketFacet {
    pub buckets: Vec<Bucket>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Bucket {
    pub val: String,
    pub count: u64,
}

impl Default for Bucket {
    fn default() -> Self {
        Self {
            val: "Unknown".to_owned(),
            count: 0,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Hits {
    pub hits: Vec<Hit>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Hit {
    pub repo: Option<RawString>,
    pub path: Option<RawString>,
    pub branch: Option<RawString>,
    pub total_matches: Option<RawTotalMatches>,
    pub content: Option<Content>,
}

impl Hit {
    pub fn repo(&self) -> &str {
        self.repo
            .as_ref()
            .and_then(|value| value.raw.as_deref())
            .unwrap_or("Unknown")
    }

    pub fn path(&self) -> &str {
        self.path
            .as_ref()
            .and_then(|value| value.raw.as_deref())
            .unwrap_or("Unknown")
    }

    pub fn branch(&self) -> &str {
        self.branch
            .as_ref()
            .and_then(|value| value.raw.as_deref())
            .unwrap_or("main")
    }

    pub fn total_matches(&self) -> &str {
        self.total_matches
            .as_ref()
            .map_or("0", |value| value.raw.as_str())
    }

    pub fn snippet(&self) -> &str {
        self.content
            .as_ref()
            .and_then(|content| content.snippet.as_deref())
            .unwrap_or("")
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RawString {
    pub raw: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct RawTotalMatches {
    #[serde(deserialize_with = "deserialize_string_or_integer")]
    pub raw: String,
}

impl Default for RawTotalMatches {
    fn default() -> Self {
        Self {
            raw: "0".to_owned(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Content {
    pub snippet: Option<String>,
}

fn deserialize_string_or_integer<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(Value::String(value)) => value,
        Some(Value::Number(value)) => value.to_string(),
        _ => "0".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::SearchResponse;

    #[test]
    fn deserializes_captured_fixture() {
        let response: SearchResponse =
            serde_json::from_str(include_str!("../../tests/fixtures/search_basic.json"))
                .expect("fixture must deserialize");
        assert_eq!(response.facets.count, 1234);
        assert_eq!(response.hits.hits.len(), 3);
        assert_eq!(response.hits.hits[0].repo(), "fastapi/fastapi");
        assert_eq!(response.hits.hits[0].total_matches(), "12");
    }

    #[test]
    fn total_matches_accepts_string_and_integer_raw_values() {
        for (raw, expected) in [(json!("5"), "5"), (json!(5), "5")] {
            let response: SearchResponse = serde_json::from_value(json!({
                "hits": { "hits": [{ "total_matches": { "raw": raw } }] }
            }))
            .expect("response must deserialize");
            assert_eq!(response.hits.hits[0].total_matches(), expected);
        }
    }

    #[test]
    fn missing_nested_fields_use_reference_defaults() {
        let response: SearchResponse = serde_json::from_value(json!({
            "hits": { "hits": [{}] }
        }))
        .expect("response must deserialize");
        let hit = &response.hits.hits[0];
        assert_eq!(hit.repo(), "Unknown");
        assert_eq!(hit.path(), "Unknown");
        assert_eq!(hit.branch(), "main");
        assert_eq!(hit.total_matches(), "0");
        assert_eq!(hit.snippet(), "");
    }
}
