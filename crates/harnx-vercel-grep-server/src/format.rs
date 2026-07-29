use std::sync::OnceLock;

use fancy_regex::Regex;
use serde::Serialize;

use crate::server::model::SearchResponse;

static HTML_TAG_REGEX: OnceLock<Result<Regex, fancy_regex::Error>> = OnceLock::new();
static LINE_NUMBER_REGEX: OnceLock<Result<Regex, fancy_regex::Error>> = OnceLock::new();

pub fn extract_text_from_html(html: &str) -> String {
    let without_tags = match HTML_TAG_REGEX.get_or_init(|| Regex::new(r"<[^>]+>")) {
        Ok(regex) => regex.replace_all(html, "").into_owned(),
        Err(error) => {
            log::error!("failed to compile HTML tag regex: {error}");
            html.to_owned()
        }
    };

    without_tags
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .trim()
        .to_owned()
}

pub fn extract_line_numbers(html: &str) -> Vec<u32> {
    let regex = match LINE_NUMBER_REGEX.get_or_init(|| Regex::new(r#"data-line="(\d+)""#)) {
        Ok(regex) => regex,
        Err(error) => {
            log::error!("failed to compile line number regex: {error}");
            return Vec::new();
        }
    };

    regex
        .captures_iter(html)
        .filter_map(|captures| match captures {
            Ok(captures) => captures
                .get(1)
                .and_then(|capture| capture.as_str().parse::<u32>().ok()),
            Err(error) => {
                log::error!("failed to extract line number: {error}");
                None
            }
        })
        .collect()
}

pub fn language_from_extension(extension: &str) -> &'static str {
    match extension {
        "py" => "python",
        "js" | "jsx" => "javascript",
        "ts" | "tsx" => "typescript",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "php" => "php",
        "rb" => "ruby",
        "go" => "go",
        "rs" => "rust",
        "swift" => "swift",
        "kt" => "kotlin",
        "scala" => "scala",
        "sh" | "bash" | "zsh" | "fish" => "bash",
        "ps1" => "powershell",
        "sql" => "sql",
        "html" | "htm" => "html",
        "xml" => "xml",
        "css" => "css",
        "scss" => "scss",
        "sass" => "sass",
        "less" => "less",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "ini" | "cfg" | "conf" => "ini",
        "md" | "markdown" => "markdown",
        "tex" => "latex",
        "r" => "r",
        "matlab" | "m" => "matlab",
        "pl" => "perl",
        "lua" => "lua",
        "vim" => "vim",
        "dockerfile" => "dockerfile",
        "makefile" | "make" => "makefile",
        _ => "text",
    }
}

pub fn format_code_snippet(text: &str, language: &str) -> String {
    if text.trim().is_empty() {
        return String::new();
    }

    let snippet = if text.chars().count() > 400 {
        let mut truncated = text.chars().take(400).collect::<String>();
        truncated.push_str("...");
        truncated
    } else {
        text.to_owned()
    };

    let lines = snippet.split('\n').collect::<Vec<_>>();
    let mut formatted_lines = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let cleaned = line.trim_end();
        if !cleaned.is_empty() {
            formatted_lines.push(cleaned);
        }
        if formatted_lines.len() >= 8 {
            if index < lines.len() - 1 {
                formatted_lines.push("... (truncated)");
            }
            break;
        }
    }

    let joined = formatted_lines.join("\n");
    if !language.is_empty() && language != "text" {
        format!("```{language}\n{joined}\n```")
    } else {
        joined
    }
}

pub fn build_output(query: &str, response: &SearchResponse) -> String {
    let top_languages = response
        .facets
        .lang
        .buckets
        .iter()
        .take(5)
        .map(|bucket| TopLanguage {
            language: if bucket.val.is_empty() {
                "Unknown"
            } else {
                &bucket.val
            },
            count: bucket.count,
        })
        .collect();
    let top_repositories = response
        .facets
        .repo
        .buckets
        .iter()
        .take(5)
        .map(|bucket| TopRepository {
            repository: if bucket.val.is_empty() {
                "Unknown"
            } else {
                &bucket.val
            },
            count: bucket.count,
        })
        .collect();

    let mut results_by_repository: Vec<RepositoryResult> = Vec::new();
    for hit in response.hits.hits.iter().take(10) {
        let repository = hit.repo();
        let path = hit.path();
        let raw_total_matches = hit.total_matches();
        let total_matches = if !raw_total_matches.is_empty()
            && raw_total_matches
                .chars()
                .all(|character| character.is_ascii_digit())
        {
            raw_total_matches.parse::<u64>().unwrap_or(0)
        } else {
            0
        };
        let raw_snippet = hit.snippet();
        let clean_snippet = extract_text_from_html(raw_snippet);
        let line_numbers = extract_line_numbers(raw_snippet);
        let extension = if path.contains('.') {
            path.rsplit('.').next().unwrap_or("txt").to_lowercase()
        } else {
            "txt".to_owned()
        };
        let language = language_from_extension(&extension);
        let file = FileResult {
            file_path: path.to_owned(),
            branch: hit.branch().to_owned(),
            total_matches,
            line_numbers,
            language,
            code_snippet: format_code_snippet(&clean_snippet, language),
        };

        if let Some(group) = results_by_repository
            .iter_mut()
            .find(|group| group.repository == repository)
        {
            group.matches_count = group.matches_count.saturating_add(total_matches);
            group.files.push(file);
        } else {
            results_by_repository.push(RepositoryResult {
                repository: repository.to_owned(),
                matches_count: total_matches,
                files: vec![file],
            });
        }
    }

    results_by_repository.sort_by_key(|repository| std::cmp::Reverse(repository.matches_count));
    let results_shown = results_by_repository
        .iter()
        .map(|repository| repository.files.len())
        .sum();
    let repositories_found = results_by_repository.len();
    let output = SearchOutput {
        query,
        summary: Summary {
            total_results: response.facets.count,
            results_shown,
            repositories_found,
            top_languages,
            top_repositories,
        },
        results_by_repository,
    };

    serialize_pretty(&output)
}

pub fn build_not_found_output(query: &str) -> String {
    serialize_pretty(&NotFoundOutput {
        query,
        summary: NotFoundSummary {
            total_results: 0,
            message: "No results found for this query",
        },
        results: Vec::new(),
    })
}

fn serialize_pretty<T: Serialize>(value: &T) -> String {
    match serde_json::to_string_pretty(value) {
        Ok(output) => output,
        Err(error) => {
            log::error!("failed to serialize grep output: {error}");
            String::new()
        }
    }
}

#[derive(Serialize)]
struct SearchOutput<'a> {
    query: &'a str,
    summary: Summary<'a>,
    results_by_repository: Vec<RepositoryResult>,
}

#[derive(Serialize)]
struct Summary<'a> {
    total_results: u64,
    results_shown: usize,
    repositories_found: usize,
    top_languages: Vec<TopLanguage<'a>>,
    top_repositories: Vec<TopRepository<'a>>,
}

#[derive(Serialize)]
struct TopLanguage<'a> {
    language: &'a str,
    count: u64,
}

#[derive(Serialize)]
struct TopRepository<'a> {
    repository: &'a str,
    count: u64,
}

#[derive(Serialize)]
struct RepositoryResult {
    repository: String,
    matches_count: u64,
    files: Vec<FileResult>,
}

#[derive(Serialize)]
struct FileResult {
    file_path: String,
    branch: String,
    total_matches: u64,
    line_numbers: Vec<u32>,
    language: &'static str,
    code_snippet: String,
}

#[derive(Serialize)]
struct NotFoundOutput<'a> {
    query: &'a str,
    summary: NotFoundSummary,
    results: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct NotFoundSummary {
    total_results: u64,
    message: &'static str,
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::{
        build_not_found_output, build_output, extract_line_numbers, extract_text_from_html,
        format_code_snippet, language_from_extension,
    };
    use crate::server::model::SearchResponse;

    const GOLDEN_SUCCESS: &str = r#"{
  "query": "FastAPI",
  "summary": {
    "total_results": 1234,
    "results_shown": 1,
    "repositories_found": 1,
    "top_languages": [
      {
        "language": "Python",
        "count": 500
      }
    ],
    "top_repositories": [
      {
        "repository": "fastapi/fastapi",
        "count": 100
      }
    ]
  },
  "results_by_repository": [
    {
      "repository": "fastapi/fastapi",
      "matches_count": 5,
      "files": [
        {
          "file_path": "src/main.py",
          "branch": "main",
          "total_matches": 5,
          "line_numbers": [
            10,
            11
          ],
          "language": "python",
          "code_snippet": "```python\nimport fastapi\napp = FastAPI()\n```"
        }
      ]
    }
  ]
}"#;

    const GOLDEN_NOT_FOUND: &str = r#"{
  "query": "somequery",
  "summary": {
    "total_results": 0,
    "message": "No results found for this query"
  },
  "results": []
}"#;

    #[test]
    fn strips_tags_decodes_entities_in_reference_order_and_trims() {
        assert_eq!(
            extract_text_from_html("  <span>&quot;x&quot; &amp; &lt;y&gt; &amp;lt;</span>  "),
            "\"x\" & <y> <"
        );
    }

    #[test]
    fn extracts_only_well_formed_numeric_line_numbers() {
        let html = r#"<span data-line="7">a</span><i data-line="42">b</i>"#;
        assert_eq!(extract_line_numbers(html), vec![7, 42]);
        assert!(extract_line_numbers("<span>none</span>").is_empty());
        assert!(extract_line_numbers(r#"data-line="12x" data-line="abc""#).is_empty());
    }

    #[test]
    fn maps_extensions_and_defaults_to_text() {
        assert_eq!(language_from_extension("py"), "python");
        assert_eq!(language_from_extension("rs"), "rust");
        assert_eq!(language_from_extension("ts"), "typescript");
        assert_eq!(language_from_extension("unknown"), "text");
        assert_eq!(language_from_extension("txt"), "text");
    }

    #[test]
    fn formats_empty_plain_and_fenced_snippets() {
        assert_eq!(format_code_snippet(" \n\t", "rust"), "");
        assert_eq!(format_code_snippet("one\n\ntwo  ", "text"), "one\ntwo");
        assert_eq!(
            format_code_snippet("fn main() {}", "rust"),
            "```rust\nfn main() {}\n```"
        );
    }

    #[test]
    fn truncates_to_400_characters_before_splitting_lines() {
        let long = "x".repeat(401);
        assert_eq!(
            format_code_snippet(&long, "text"),
            format!("{}...", "x".repeat(400))
        );

        let split_after_boundary = format!("{}\nsecond line", "a".repeat(399));
        assert_eq!(
            format_code_snippet(&split_after_boundary, "text"),
            format!("{}\n...", "a".repeat(399))
        );
    }

    #[test]
    fn keeps_eight_non_empty_lines_then_marks_truncation() {
        let snippet = "one\n\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine";
        assert_eq!(
            format_code_snippet(snippet, "text"),
            "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n... (truncated)"
        );
    }

    #[test]
    fn golden_success_output_is_byte_exact() {
        let response: SearchResponse =
            serde_json::from_str(include_str!("../tests/fixtures/search_golden_single.json"))
                .expect("golden fixture must deserialize");
        assert_eq!(build_output("FastAPI", &response), GOLDEN_SUCCESS);
    }

    #[test]
    fn basic_output_groups_sorts_and_summarizes_results() {
        let response: SearchResponse =
            serde_json::from_str(include_str!("../tests/fixtures/search_basic.json"))
                .expect("basic fixture must deserialize");
        let output: Value =
            serde_json::from_str(&build_output("FastAPI", &response)).expect("valid output JSON");

        assert_eq!(output["summary"]["total_results"], 1234);
        assert_eq!(output["summary"]["results_shown"], 3);
        assert_eq!(output["summary"]["repositories_found"], 2);
        assert_eq!(
            output["summary"]["top_languages"].as_array().unwrap().len(),
            3
        );
        assert_eq!(
            output["summary"]["top_repositories"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            output["results_by_repository"][0]["repository"],
            "fastapi/fastapi"
        );
        assert_eq!(output["results_by_repository"][0]["matches_count"], 15);
        assert_eq!(
            output["results_by_repository"][1]["repository"],
            "tiangolo/full-stack-fastapi-template"
        );
        assert_eq!(output["results_by_repository"][1]["matches_count"], 5);
    }

    #[test]
    fn no_extension_path_uses_plain_text_snippet() {
        let response: SearchResponse = serde_json::from_value(serde_json::json!({
            "hits": { "hits": [{
                "repo": { "raw": "owner/repo" },
                "path": { "raw": "README" },
                "content": { "snippet": "<span data-line=\"1\">plain text</span>" }
            }] }
        }))
        .expect("response must deserialize");
        let output: Value =
            serde_json::from_str(&build_output("plain", &response)).expect("valid output JSON");
        let file = &output["results_by_repository"][0]["files"][0];
        assert_eq!(file["language"], "text");
        assert_eq!(file["code_snippet"], "plain text");
    }

    #[test]
    fn limits_summary_facets_to_top_five_buckets() {
        let response: SearchResponse = serde_json::from_value(serde_json::json!({
            "facets": {
                "lang": { "buckets": [
                    {"val": "one"}, {"val": "two"}, {"val": "three"},
                    {"val": "four"}, {"val": "five"}, {"val": "six"}
                ]},
                "repo": { "buckets": [
                    {"val": "r/1"}, {"val": "r/2"}, {"val": "r/3"},
                    {"val": "r/4"}, {"val": "r/5"}, {"val": "r/6"}
                ]}
            }
        }))
        .expect("response must deserialize");
        let output: Value =
            serde_json::from_str(&build_output("query", &response)).expect("valid output JSON");
        assert_eq!(
            output["summary"]["top_languages"].as_array().unwrap().len(),
            5
        );
        assert_eq!(
            output["summary"]["top_repositories"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
        assert_eq!(output["summary"]["top_languages"][4]["language"], "five");
        assert_eq!(
            output["summary"]["top_repositories"][4]["repository"],
            "r/5"
        );
    }

    #[test]
    fn not_found_output_is_byte_exact() {
        assert_eq!(build_not_found_output("somequery"), GOLDEN_NOT_FOUND);
    }
}
