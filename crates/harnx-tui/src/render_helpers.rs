use harnx_core::event::AgentSource;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// Render one line of MCP tool template text into ratatui spans, applying
/// `base_style` as the foreground/modifier base. Delegates to the
/// `tui-markdown` crate (which wraps `pulldown-cmark` + `syntect` +
/// `ansi-to-tui` internally), so we get the same inline emphasis handling
/// (`**bold**`, `*italic*`, `` `code` ``) without maintaining our own
/// parser.
///
/// On render failure (empty result), returns the input as a single plain
/// span so the user still sees the text — markdown styling is a
/// presentation nicety, not a correctness requirement.
pub(crate) fn markdown_line_spans(text: &str, base_style: Style) -> Line<'static> {
    let plain_fallback = || Line::from(Span::styled(text.to_string(), base_style));

    // `tui_markdown::from_str` returns a `Text` with zero or more lines.
    // For a single input line we expect exactly one parsed line; any
    // additional lines (which shouldn't happen for inline markdown) are
    // dropped — caller is expected to split the input on `\n` first.
    let parsed = tui_markdown::from_str(text);
    let first = match parsed.into_iter().next() {
        Some(line) if !line.spans.is_empty() => line,
        _ => return plain_fallback(),
    };

    // Patch `base_style` under each parsed span so the dim/grey context
    // (set by the caller) applies wherever the parsed span doesn't
    // explicitly override it. `Style::patch` keeps the right-hand side's
    // explicit fields and falls through to the left for `None` ones.
    let spans: Vec<Span<'static>> = first
        .spans
        .into_iter()
        .map(|span| {
            let merged = base_style.patch(span.style);
            Span::styled(span.content.into_owned(), merged)
        })
        .collect();
    Line::from(spans)
}

pub(crate) fn render_status_line(title: Option<&str>, status: Option<&str>) -> Option<String> {
    let line = [title, status]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    (!line.is_empty()).then_some(format!("-> {line}"))
}

pub(crate) fn source_heading(source: &AgentSource) -> String {
    match &source.session_id {
        Some(session_id) if !session_id.is_empty() => {
            format!("> {} ▸ {}", source.agent, session_id)
        }
        _ => format!("> {}", source.agent),
    }
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
    fn italic_asterisk_produces_italic_span() {
        let line = markdown_line_spans("hi *there* you", Style::default());
        assert_eq!(span_text(&line), "hi there you");
        let it = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "there")
            .unwrap();
        assert!(it.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn italic_underscore_produces_italic_span() {
        let line = markdown_line_spans("hi _there_ you", Style::default());
        let it = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "there")
            .unwrap();
        assert!(it.style.add_modifier.contains(Modifier::ITALIC));
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
        // Mirror the actual built-in bash exec template:
        //   "**$** `{{ args.command }}`"
        // After Jinja rendering this becomes "**$** `ls -la /tmp`"
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
}
