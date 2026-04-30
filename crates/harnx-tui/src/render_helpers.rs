use harnx_core::event::AgentSource;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Render a single line of inline markdown into ratatui spans, applying
/// `base_style` as the foreground color/modifier and adding bold/italic/
/// code styling on top. Supports the small subset MCP tool templates use:
///
/// - `**bold**`
/// - `*italic*` and `_italic_`
/// - `` `code` ``
///
/// Backslash escapes any marker (`\*`, `\_`, `` \` ``, `\\`). Unmatched
/// markers render literally. Block-level constructs (headings, lists,
/// fenced code) are not parsed — caller is expected to split on `\n` and
/// call this per line.
pub(crate) fn markdown_line_spans(text: &str, base_style: Style) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if i + 1 < chars.len() && is_marker(chars[i + 1]) => {
                buf.push(chars[i + 1]);
                i += 2;
            }
            '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                if let Some(end) = find_close(&chars, i + 2, "**") {
                    flush(&mut spans, &mut buf, base_style);
                    let inner = collect_with_escapes(&chars, i + 2, end);
                    let mut bold = base_style;
                    bold.add_modifier.insert(Modifier::BOLD);
                    spans.push(Span::styled(inner, bold));
                    i = end + 2;
                } else {
                    buf.push('*');
                    buf.push('*');
                    i += 2;
                }
            }
            '*' | '_' => {
                let needle = c.to_string();
                if let Some(end) = find_close(&chars, i + 1, &needle) {
                    flush(&mut spans, &mut buf, base_style);
                    let inner = collect_with_escapes(&chars, i + 1, end);
                    let mut italic = base_style;
                    italic.add_modifier.insert(Modifier::ITALIC);
                    spans.push(Span::styled(inner, italic));
                    i = end + 1;
                } else {
                    buf.push(c);
                    i += 1;
                }
            }
            '`' => {
                if let Some(end) = find_close(&chars, i + 1, "`") {
                    flush(&mut spans, &mut buf, base_style);
                    let inner: String = chars[i + 1..end].iter().collect();
                    let code_style = base_style.fg(Color::Yellow);
                    spans.push(Span::styled(inner, code_style));
                    i = end + 1;
                } else {
                    buf.push('`');
                    i += 1;
                }
            }
            _ => {
                buf.push(c);
                i += 1;
            }
        }
    }
    flush(&mut spans, &mut buf, base_style);
    Line::from(spans)
}

fn is_marker(c: char) -> bool {
    matches!(c, '*' | '_' | '`' | '\\')
}

fn flush(spans: &mut Vec<Span<'static>>, buf: &mut String, style: Style) {
    if !buf.is_empty() {
        spans.push(Span::styled(std::mem::take(buf), style));
    }
}

/// Find the next index in `chars[from..]` where `needle` (a 1- or 2-char
/// marker) starts, treating backslash-escaped markers as literal.
fn find_close(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    let needle_chars: Vec<char> = needle.chars().collect();
    let mut i = from;
    while i + needle_chars.len() <= chars.len() {
        // Skip escapes: `\X` consumes two chars and is never a closing marker.
        if chars[i] == '\\' && i + 1 < chars.len() && is_marker(chars[i + 1]) {
            i += 2;
            continue;
        }
        if chars[i..i + needle_chars.len()] == *needle_chars {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Collect `chars[from..to]` into a String, processing backslash escapes.
fn collect_with_escapes(chars: &[char], from: usize, to: usize) -> String {
    let mut out = String::new();
    let mut i = from;
    while i < to {
        if chars[i] == '\\' && i + 1 < to && is_marker(chars[i + 1]) {
            out.push(chars[i + 1]);
            i += 2;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
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
    use super::*;

    fn span_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    fn span_modifiers(line: &Line<'static>) -> Vec<(String, Modifier)> {
        line.spans
            .iter()
            .map(|s| (s.content.to_string(), s.style.add_modifier))
            .collect()
    }

    #[test]
    fn plain_text_passes_through() {
        let line = markdown_line_spans("hello world", Style::default());
        assert_eq!(span_text(&line), "hello world");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].style.add_modifier, Modifier::empty());
    }

    #[test]
    fn bold_marker_produces_bold_span() {
        let line = markdown_line_spans("hi **there** you", Style::default());
        assert_eq!(span_text(&line), "hi there you");
        let mods = span_modifiers(&line);
        let bold_part = mods
            .iter()
            .find(|(t, _)| t == "there")
            .expect("expected 'there' span");
        assert!(
            bold_part.1.contains(Modifier::BOLD),
            "expected BOLD on 'there'; got {:?}",
            bold_part.1
        );
    }

    #[test]
    fn italic_asterisk_produces_italic_span() {
        let line = markdown_line_spans("hi *there* you", Style::default());
        assert_eq!(span_text(&line), "hi there you");
        let mods = span_modifiers(&line);
        let it = mods.iter().find(|(t, _)| t == "there").unwrap();
        assert!(it.1.contains(Modifier::ITALIC));
    }

    #[test]
    fn italic_underscore_produces_italic_span() {
        let line = markdown_line_spans("hi _there_ you", Style::default());
        let mods = span_modifiers(&line);
        let it = mods.iter().find(|(t, _)| t == "there").unwrap();
        assert!(it.1.contains(Modifier::ITALIC));
    }

    #[test]
    fn code_marker_produces_yellow_span() {
        let line = markdown_line_spans("run `ls -la`", Style::default());
        assert_eq!(span_text(&line), "run ls -la");
        let code_span = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "ls -la")
            .expect("expected code span");
        assert_eq!(code_span.style.fg, Some(Color::Yellow));
    }

    #[test]
    fn unmatched_marker_renders_literally() {
        let line = markdown_line_spans("a * b _ c ` d", Style::default());
        assert_eq!(span_text(&line), "a * b _ c ` d");
        // No styled span should have BOLD/ITALIC.
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
        let mods = span_modifiers(&line);
        let bold = mods.iter().find(|(t, _)| t == "$").unwrap();
        assert!(bold.1.contains(Modifier::BOLD));
        let code = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "ls -la /tmp")
            .unwrap();
        assert_eq!(code.style.fg, Some(Color::Yellow));
    }

    #[test]
    fn backslash_escape_keeps_marker_literal() {
        let line = markdown_line_spans("not \\*bold\\* here", Style::default());
        assert_eq!(span_text(&line), "not *bold* here");
        for s in &line.spans {
            assert!(!s.style.add_modifier.contains(Modifier::BOLD));
            assert!(!s.style.add_modifier.contains(Modifier::ITALIC));
        }
    }

    #[test]
    fn base_style_propagates_to_unstyled_runs() {
        let base = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);
        let line = markdown_line_spans("hi **bold** world", base);
        for span in &line.spans {
            // Every span should keep the DarkGray + DIM base, plus any extra modifiers.
            assert_eq!(span.style.fg, Some(Color::DarkGray));
            assert!(span.style.add_modifier.contains(Modifier::DIM));
        }
        // The 'bold' span should additionally be BOLD.
        let bold = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "bold")
            .unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }
}
