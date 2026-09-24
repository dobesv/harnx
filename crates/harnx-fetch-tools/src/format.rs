use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;

use dom_smoothie::Readability;
use html_to_markdown::{markdown, TagHandler};
use regex::Regex;
use scraper::{Html, Selector};

pub fn html_to_md(html: &str) -> String {
    let mut handlers: Vec<TagHandler> = vec![
        Rc::new(RefCell::new(markdown::ParagraphHandler)),
        Rc::new(RefCell::new(markdown::HeadingHandler)),
        Rc::new(RefCell::new(markdown::ListHandler)),
        Rc::new(RefCell::new(markdown::TableHandler::new())),
        Rc::new(RefCell::new(markdown::StyledTextHandler)),
        Rc::new(RefCell::new(markdown::CodeHandler)),
        Rc::new(RefCell::new(markdown::WebpageChromeRemover)),
    ];

    html_to_markdown::convert_html_to_markdown(html.as_bytes(), &mut handlers)
        .unwrap_or_else(|_| html.to_owned())
}

pub fn html_to_text(html: &str) -> Result<String, String> {
    static SCRIPT_STYLE: OnceLock<Regex> = OnceLock::new();
    let regex = SCRIPT_STYLE.get_or_init(|| {
        Regex::new(r"(?is)<(?:script|style)\b[^>]*>.*?</(?:script|style)\s*>")
            .expect("valid script/style regex")
    });
    let cleaned = regex.replace_all(html, " ");
    let document = Html::parse_document(&cleaned);
    let selector = Selector::parse("body").expect("valid body selector");
    let text = if let Some(body) = document.select(&selector).next() {
        body.text().collect::<Vec<_>>().join(" ")
    } else {
        document.root_element().text().collect::<Vec<_>>().join(" ")
    };
    Ok(collapse_whitespace(&text))
}

pub fn readable_markdown(html: &str, url: &str) -> Result<String, String> {
    let mut readability = Readability::new(html, Some(url), None)
        .map_err(|error| format!("readability setup failed: {error}"))?;
    let article = readability
        .parse()
        .map_err(|error| format!("readability extraction failed: {error}"))?;
    if article.content.trim().is_empty() || article.text_content.trim().is_empty() {
        return Err("readability extraction found no article content".to_owned());
    }
    let markdown = html_to_md(article.content.as_ref());
    if markdown.trim().is_empty() {
        return Err("readability extraction produced empty Markdown".to_owned());
    }
    Ok(markdown)
}

pub fn player_response_json(page: &str) -> Result<&str, String> {
    let marker = "ytInitialPlayerResponse";
    let marker_end = page
        .find(marker)
        .map(|index| index + marker.len())
        .ok_or_else(|| {
            "YouTube player response was not found (page may require consent or bot verification)"
                .to_owned()
        })?;
    let rest = &page[marker_end..];
    let start_offset = rest
        .find('{')
        .ok_or_else(|| "YouTube player response JSON did not start with an object".to_owned())?;
    let json = &rest[start_offset..];
    // Let serde consume exactly one JSON value and report where it ended, rather
    // than hand-rolling a brace scanner. `byte_offset()` after the first item is
    // the index just past the object's closing `}`, so this handles strings,
    // escapes, and nested objects correctly.
    let mut stream = serde_json::Deserializer::from_str(json).into_iter::<serde_json::Value>();
    match stream.next() {
        Some(Ok(_)) => Ok(&json[..stream.byte_offset()]),
        _ => Err("YouTube player response JSON was incomplete".to_owned()),
    }
}

pub fn transcript_lines(xml: &str) -> Result<String, String> {
    let document = Html::parse_document(xml);
    let selector = Selector::parse("text, p").expect("valid transcript selector");
    let mut lines = Vec::new();
    for element in document.select(&selector) {
        let attrs = element.value();
        let start = if let Some(seconds) = attrs.attr("start") {
            seconds.parse::<f64>().ok()
        } else {
            attrs
                .attr("t")
                .and_then(|milliseconds| milliseconds.parse::<f64>().ok())
                .map(|milliseconds| milliseconds / 1000.0)
        };
        let Some(start) = start else {
            continue;
        };
        let text = collapse_whitespace(&element.text().collect::<Vec<_>>().join(" "));
        if text.is_empty() {
            continue;
        }
        let total_seconds = start.max(0.0).floor() as u64;
        lines.push(format!(
            "[{:02}:{:02}] {text}",
            total_seconds / 60,
            total_seconds % 60
        ));
    }
    if lines.is_empty() {
        return Err("caption response contained no transcript lines".to_owned());
    }
    Ok(lines.join("\n"))
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_extraction_removes_scripts_styles_and_collapses_space() {
        let html = "<html><body>Hello <script>bad()</script><style>.bad{}</style> world\n now</body></html>";
        assert_eq!(html_to_text(html).unwrap(), "Hello world now");
    }

    #[test]
    fn parses_balanced_player_json_with_braces_in_strings() {
        let page =
            r#"<script>ytInitialPlayerResponse = {"value":"} {","nested":{"ok":true}};</script>"#;
        let value: serde_json::Value =
            serde_json::from_str(player_response_json(page).unwrap()).unwrap();
        assert_eq!(value["nested"]["ok"], true);
    }

    #[test]
    fn formats_both_caption_xml_shapes() {
        let xml = r#"<transcript><text start="1.2" dur="2">Hello &amp; world</text><p t="65000" d="1000">Later</p></transcript>"#;
        assert_eq!(
            transcript_lines(xml).unwrap(),
            "[00:01] Hello & world\n[01:05] Later"
        );
    }
}
