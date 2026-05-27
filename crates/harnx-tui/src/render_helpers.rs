use harnx_core::event::AgentSource;

#[cfg(test)]
use ratatui::text::{Line, Span};

#[cfg(test)]
use ratatui::style::Style;

/// Render one line of inline markdown into ratatui spans, applying
/// `base_style` as the foreground/modifier base. Uses the new
/// `render_markdown` module for consistent rendering.
///
/// On render failure (empty result), returns the input as a single plain
/// span so the user still sees the text — markdown styling is a
/// presentation nicety, not a correctness requirement.
///
/// Only used in tests.
#[cfg(test)]
pub(crate) fn markdown_line_spans(text: &str, base_style: Style) -> Line<'static> {
    let plain_fallback = || Line::from(Span::styled(text.to_string(), base_style));
    let entry = crate::markdown_render::render_markdown(text, base_style, 120);
    match entry.blocks.into_iter().next() {
        Some(crate::markdown_render::MarkdownBlockData::Paragraph { lines, .. }) => {
            lines.into_iter().next().unwrap_or_else(plain_fallback)
        }
        _ => plain_fallback(),
    }
}

pub(crate) fn render_status_line(markdown: Option<&str>, status: Option<&str>) -> Option<String> {
    let line = [markdown, status]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!line.is_empty()).then_some(format!("-> {line}"))
}

pub(crate) fn source_heading(source: &AgentSource) -> String {
    source.heading()
}

pub(crate) fn render_usage_line(
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    session_label: Option<&str>,
    source: Option<&AgentSource>,
) -> Option<String> {
    let mut parts = vec![];
    if let Some(label) = session_label {
        parts.push(label.to_string());
    } else if let Some(source) = source {
        parts.push(source_heading(source));
    }
    if input_tokens > 0 {
        parts.push(format!("in {input_tokens}"));
    }
    if output_tokens > 0 {
        parts.push(format!("out {output_tokens}"));
    }
    if cached_tokens > 0 {
        parts.push(format!("cache {cached_tokens}"));
    }
    (!parts.is_empty()).then(|| parts.join("   "))
}

#[cfg(test)]
mod markdown_tests {
    //! These tests pin the *behaviors* the templating system relies on:
    //! markers stripped, content preserved, BOLD/ITALIC modifiers attached
    //! to emphasized text, and a non-default style on inline code. They do
    //! not assert specific colors — the underlying `tui-markdown` crate
    //! picks those, and we don't want to break on cosmetic changes there.
    use super::*;
    use ratatui::style::{Color, Modifier};

    fn span_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn plain_text_passes_through() {
        let line = markdown_line_spans("hello world", Style::default());
        assert_eq!(span_text(&line), "hello world");
        for span in &line.spans {
            assert!(!span.style.add_modifier.contains(Modifier::BOLD));
            assert!(!span.style.add_modifier.contains(Modifier::ITALIC));
        }
    }

    #[test]
    fn bold_marker_produces_bold_span() {
        let line = markdown_line_spans("hi **there** you", Style::default());
        assert_eq!(span_text(&line), "hi there you");
        let bold = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "there")
            .expect("expected 'there' span");
        assert!(
            bold.style.add_modifier.contains(Modifier::BOLD),
            "expected BOLD on 'there'; got {:?}",
            bold.style.add_modifier
        );
    }

    #[test]
    fn italic_marker_produces_italic_span() {
        // Both `*text*` and `_text_` should produce an ITALIC-modifier span.
        for input in ["hi *there* you", "hi _there_ you"] {
            let line = markdown_line_spans(input, Style::default());
            assert_eq!(span_text(&line), "hi there you", "input: {input}");
            let it = line
                .spans
                .iter()
                .find(|s| s.content.as_ref() == "there")
                .unwrap_or_else(|| panic!("expected `there` span for {input}"));
            assert!(
                it.style.add_modifier.contains(Modifier::ITALIC),
                "{input} should produce ITALIC; got {:?}",
                it.style.add_modifier
            );
        }
    }

    #[test]
    fn code_marker_produces_styled_span() {
        let line = markdown_line_spans("run `ls -la`", Style::default());
        assert_eq!(span_text(&line), "run ls -la");
        let code = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "ls -la")
            .expect("expected code span");
        // Inline code should be visually distinct — it carries either an
        // explicit fg or bg from `tui-markdown`. The exact color is left
        // to that crate; we only require it isn't bare default.
        assert!(
            code.style.fg.is_some() || code.style.bg.is_some(),
            "code span should be visually distinct; got {:?}",
            code.style
        );
    }

    #[test]
    fn unmatched_marker_renders_literally() {
        let line = markdown_line_spans("a * b _ c ` d", Style::default());
        assert_eq!(span_text(&line), "a * b _ c ` d");
        for s in &line.spans {
            assert!(!s.style.add_modifier.contains(Modifier::BOLD));
            assert!(!s.style.add_modifier.contains(Modifier::ITALIC));
        }
    }

    #[test]
    fn bash_template_example_renders_bold_and_code() {
        // Test inline code span rendering (used by older single-line templates
        // and any template that produces inline backtick markup).
        // "**$** `ls -la /tmp`" exercises both bold and inline code styling.
        let line = markdown_line_spans("**$** `ls -la /tmp`", Style::default());
        assert_eq!(span_text(&line), "$ ls -la /tmp");
        let bold = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "$")
            .unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let code = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "ls -la /tmp")
            .unwrap();
        assert!(code.style.fg.is_some() || code.style.bg.is_some());
    }

    #[test]
    fn multi_line_preserves_each_input_newline() {
        // Each newline in the source should become a separate line
        use crate::markdown_render::{render_markdown, MarkdownBlockData};

        let entry = render_markdown("line-01\nline-02\nline-03", Style::default(), 120);
        let texts: Vec<String> = entry
            .blocks
            .iter()
            .flat_map(|b| match b {
                MarkdownBlockData::Paragraph { lines, .. } => lines
                    .iter()
                    .map(|l| {
                        l.spans
                            .iter()
                            .map(|s| s.content.as_ref())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>(),
                MarkdownBlockData::Table { .. } => vec![],
            })
            .collect();
        assert!(
            texts.iter().any(|t| t == "line-01"),
            "expected `line-01` as its own line; got {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t == "line-03"),
            "expected `line-03` as its own line; got {texts:?}"
        );
        // No "line-01 line-02 line-03" reflowed paragraph.
        assert!(
            !texts
                .iter()
                .any(|t| t.contains("line-01") && t.contains("line-02")),
            "lines were collapsed instead of preserved: {texts:?}"
        );
    }

    #[test]
    fn multi_line_keeps_paragraph_breaks() {
        // `\n\n` is a paragraph break — should produce separate blocks
        use crate::markdown_render::render_markdown;

        let entry = render_markdown("para1\n\npara2", Style::default(), 120);
        // Check that there are two blocks (two paragraphs)
        assert!(
            entry.blocks.len() >= 2,
            "expected at least 2 blocks for para1\n\npara2; got {} blocks",
            entry.blocks.len()
        );
    }

    #[test]
    fn multi_line_renders_inline_emphasis() {
        // Emphasis still works across lines.
        use crate::markdown_render::{render_markdown, MarkdownBlockData};

        let entry = render_markdown("first line\n**bold line**", Style::default(), 120);
        let bold = entry
            .blocks
            .iter()
            .flat_map(|b| match b {
                MarkdownBlockData::Paragraph { lines, .. } => lines
                    .iter()
                    .flat_map(|l| l.spans.iter())
                    .collect::<Vec<_>>(),
                MarkdownBlockData::Table { .. } => vec![],
            })
            .find(|s| s.content.as_ref() == "bold line")
            .expect("expected bold span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn render_status_line_ignores_whitespace_only_parts() {
        use super::render_status_line;
        // whitespace-only markdown → should not produce "->  " garbage
        assert_eq!(render_status_line(Some("  "), None), None);
        assert_eq!(render_status_line(None, Some("\t")), None);
        assert_eq!(render_status_line(Some("  "), Some("  ")), None);
        // real content is preserved
        assert_eq!(
            render_status_line(Some("  hello  "), None),
            Some("-> hello".to_string())
        );
        // whitespace-only part is excluded, real part kept
        assert_eq!(
            render_status_line(Some("  "), Some("running")),
            Some("-> running".to_string())
        );
    }

    #[test]
    fn base_style_propagates_to_unstyled_runs() {
        // `Style::patch` keeps the parsed span's explicit fields and falls
        // through to the base for unset ones. So an unstyled run should
        // inherit both fg=DarkGray and DIM from the base; emphasized spans
        // keep their own fg but should still pick up DIM.
        let base = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);
        let line = markdown_line_spans("hi **bold** world", base);

        // Find the unstyled "hi " span and check it inherits the base.
        let unstyled = line
            .spans
            .iter()
            .find(|s| s.content.as_ref().contains("hi"))
            .expect("expected an unstyled run");
        assert_eq!(unstyled.style.fg, Some(Color::DarkGray));
        assert!(unstyled.style.add_modifier.contains(Modifier::DIM));

        // The "bold" span should still be BOLD on top of the base DIM.
        let bold = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "bold")
            .unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        assert!(bold.style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn markdown_lines_filters_out_fence_markers() {
        // Issue #434: code fence marker lines (```sh, ```) must be stripped
        // so they don't appear as literal text.
        use crate::markdown_render::{render_markdown, MarkdownBlockData};

        let input = "```rust\nfn main() {}\n```";
        let entry = render_markdown(input, Style::default(), 120);
        let texts: Vec<String> = entry
            .blocks
            .iter()
            .flat_map(|b| match b {
                MarkdownBlockData::Paragraph { lines, .. } => lines
                    .iter()
                    .map(|l| {
                        l.spans
                            .iter()
                            .map(|s| s.content.as_ref())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>(),
                MarkdownBlockData::Table { .. } => vec![],
            })
            .collect();

        // Code block content must appear.
        assert!(
            texts.iter().any(|t| t.contains("fn main()")),
            "code content missing: {texts:?}"
        );
        // Fence markers must not appear.
        assert!(
            !texts.iter().any(|t| t.trim().starts_with("```")),
            "fence markers leaked: {texts:?}"
        );
    }
    #[test]
    fn test_source_heading() {
        let mut source = AgentSource {
            agent: "bot".to_string(),
            session_id: None,
            model: None,
        };
        assert_eq!(super::source_heading(&source), "> bot");

        source.session_id = Some("s1".to_string());
        assert_eq!(super::source_heading(&source), "> bot ▸ s1");

        source.model = Some("m1".to_string());
        assert_eq!(super::source_heading(&source), "> bot ▸ m1 ▸ s1");

        source.session_id = None;
        assert_eq!(super::source_heading(&source), "> bot ▸ m1");
    }
}
