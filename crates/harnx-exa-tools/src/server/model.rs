use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    #[serde(rename = "searchTime")]
    pub search_time: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SearchResult {
    pub title: Option<String>,
    pub url: String,
    #[serde(rename = "publishedDate")]
    pub published_date: Option<String>,
    pub author: Option<String>,
    pub highlights: Vec<String>,
    pub text: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ContentsResponse {
    pub results: Vec<ContentResult>,
    pub statuses: Vec<UrlStatus>,
    #[serde(rename = "searchTime")]
    pub search_time: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ContentResult {
    pub title: Option<String>,
    pub url: String,
    #[serde(rename = "publishedDate")]
    pub published_date: Option<String>,
    pub author: Option<String>,
    pub text: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UrlStatus {
    pub id: String,
    pub status: String,
    pub error: Option<UrlError>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UrlError {
    pub tag: Option<String>,
}

impl UrlStatus {
    pub fn error_tag(&self) -> &str {
        self.error
            .as_ref()
            .and_then(|error| error.tag.as_deref())
            .unwrap_or("unknown error")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_fixtures_deserialize() {
        let search: SearchResponse =
            serde_json::from_str(include_str!("../../tests/fixtures/search_response.json"))
                .unwrap();
        assert_eq!(search.results.len(), 3);

        let contents: ContentsResponse =
            serde_json::from_str(include_str!("../../tests/fixtures/contents_response.json"))
                .unwrap();
        assert_eq!(contents.results.len(), 2);
        assert_eq!(contents.statuses.len(), 1);
    }
}
