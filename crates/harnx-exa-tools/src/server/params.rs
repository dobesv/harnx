use serde::{Deserialize, Deserializer};

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct WebSearchParams {
    pub query: String,
    #[serde(rename = "numResults", default)]
    pub num_results: Option<u64>,
}

impl WebSearchParams {
    pub fn validate(&self) -> Result<(), String> {
        if self.query.trim().is_empty() {
            return Err(
                "❌ Error: 'query' parameter is required and must be a non-empty string".into(),
            );
        }
        Ok(())
    }

    pub fn result_count(&self) -> u64 {
        self.num_results.unwrap_or(10)
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct WebFetchParams {
    #[serde(deserialize_with = "deserialize_urls")]
    pub urls: Vec<String>,
    #[serde(rename = "maxCharacters", default)]
    pub max_characters: Option<u64>,
}

impl WebFetchParams {
    pub fn validate(&self) -> Result<(), String> {
        if self.urls.is_empty() || self.urls.iter().any(|url| url.trim().is_empty()) {
            return Err("❌ Error: 'urls' parameter is required and must contain at least one non-empty URL".into());
        }
        if self.max_characters == Some(0) {
            return Err("❌ Error: 'maxCharacters' must be at least 1".into());
        }
        Ok(())
    }

    pub fn character_limit(&self) -> u64 {
        self.max_characters.unwrap_or(3000)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum UrlInput {
    List(Vec<String>),
    String(String),
}

fn deserialize_urls<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    match UrlInput::deserialize(deserializer)? {
        UrlInput::List(urls) => Ok(urls),
        UrlInput::String(value) => {
            if let Ok(urls) = serde_json::from_str::<Vec<String>>(&value) {
                Ok(urls)
            } else {
                Ok(vec![value])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_search_result_count() {
        let params: WebSearchParams = serde_json::from_value(json!({"query": "rust"})).unwrap();
        assert_eq!(params.result_count(), 10);
    }

    #[test]
    fn accepts_url_array_json_string_and_single_url() {
        for (value, expected) in [
            (json!(["https://one.test", "https://two.test"]), 2),
            (json!(r#"["https://one.test","https://two.test"]"#), 2),
            (json!("https://one.test"), 1),
        ] {
            let params: WebFetchParams = serde_json::from_value(json!({"urls": value})).unwrap();
            assert_eq!(params.urls.len(), expected);
        }
    }

    #[test]
    fn validates_required_values_and_minimum_character_limit() {
        let search = WebSearchParams {
            query: "  ".into(),
            num_results: None,
        };
        assert!(search.validate().unwrap_err().contains("query"));

        let fetch = WebFetchParams {
            urls: vec![],
            max_characters: None,
        };
        assert!(fetch.validate().unwrap_err().contains("urls"));
        let fetch = WebFetchParams {
            urls: vec!["https://example.com".into()],
            max_characters: Some(0),
        };
        assert!(fetch.validate().unwrap_err().contains("at least 1"));
    }
}
