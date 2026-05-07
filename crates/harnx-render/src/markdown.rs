use crate::decode_bin;

use ansi_colours::AsRGB;
use anyhow::{anyhow, Context, Result};
use crossterm::style::{Color, Stylize};
use crossterm::terminal;
use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
use std::collections::HashMap;
use std::iter;
use std::mem;
use std::sync::LazyLock;
use syntect::highlighting::{Color as SyntectColor, FontStyle, Style, Theme};
use syntect::parsing::SyntaxSet;
use syntect::{easy::HighlightLines, parsing::SyntaxReference};

/// Comes from <https://github.com/sharkdp/bat/raw/5e77ca37e89c873e4490b42ff556370dc5c6ba4f/assets/syntaxes.bin>
const SYNTAXES: &[u8] = include_bytes!("../assets/syntaxes.bin");

static LANG_MAPS: LazyLock<HashMap<String, String>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    m.insert("csharp".into(), "C#".into());
    m.insert("php".into(), "PHP Source".into());
    m
});

pub struct MarkdownRender {
    options: RenderOptions,
    syntax_set: SyntaxSet,
    code_color: Option<Color>,
    md_syntax: SyntaxReference,
    code_syntax: Option<SyntaxReference>,
    prev_line_type: LineType,
    wrap_width: Option<u16>,
}

impl MarkdownRender {
    pub fn init(options: RenderOptions) -> Result<Self> {
        let syntax_set: SyntaxSet =
            decode_bin(SYNTAXES).with_context(|| "MarkdownRender: invalid syntaxes binary")?;

        let code_color = options
            .theme
            .as_ref()
            .map(|theme| get_code_color(theme, options.truecolor));
        let md_syntax = syntax_set.find_syntax_by_extension("md").unwrap().clone();
        let line_type = LineType::Normal;
        let wrap_width = match options.wrap.as_deref() {
            None => None,
            Some(value) => match terminal::size() {
                Ok((columns, _)) => {
                    if value == "auto" {
                        Some(columns)
                    } else {
                        let value = value
                            .parse::<u16>()
                            .map_err(|_| anyhow!("Invalid wrap value"))?;
                        Some(columns.min(value))
                    }
                }
                Err(_) => None,
            },
        };
        Ok(Self {
            syntax_set,
            code_color,
            md_syntax,
            code_syntax: None,
            prev_line_type: line_type,
            wrap_width,
            options,
        })
    }

    pub fn render(&mut self, text: &str) -> String {
        let preprocessed = preprocess_tables(text, self.wrap_width.map(usize::from));
        preprocessed
            .split('\n')
            .map(|line| self.render_line_mut(line))
            .collect::<Vec<String>>()
            .join("\n")
    }

    pub fn render_line(&self, line: &str) -> String {
        let (_, code_syntax, is_code) = self.check_line(line);
        if is_code {
            self.highlight_code_line(line, &code_syntax)
        } else {
            self.highlight_line(line, &self.md_syntax, false)
        }
    }

    fn render_line_mut(&mut self, line: &str) -> String {
        let (line_type, code_syntax, is_code) = self.check_line(line);
        let output = if is_code {
            self.highlight_code_line(line, &code_syntax)
        } else {
            self.highlight_line(line, &self.md_syntax, false)
        };
        self.prev_line_type = line_type;
        self.code_syntax = code_syntax;
        output
    }

    fn check_line(&self, line: &str) -> (LineType, Option<SyntaxReference>, bool) {
        let mut line_type = self.prev_line_type;
        let mut code_syntax = self.code_syntax.clone();
        let mut is_code = false;
        if let Some(lang) = detect_code_block(line) {
            match line_type {
                LineType::Normal | LineType::CodeEnd => {
                    line_type = LineType::CodeBegin;
                    code_syntax = if lang.is_empty() {
                        None
                    } else {
                        self.find_syntax(&lang).cloned()
                    };
                }
                LineType::CodeBegin | LineType::CodeInner => {
                    line_type = LineType::CodeEnd;
                    code_syntax = None;
                }
            }
        } else {
            match line_type {
                LineType::Normal => {}
                LineType::CodeEnd => {
                    line_type = LineType::Normal;
                }
                LineType::CodeBegin => {
                    if code_syntax.is_none() {
                        if let Some(syntax) = self.syntax_set.find_syntax_by_first_line(line) {
                            code_syntax = Some(syntax.clone());
                        }
                    }
                    line_type = LineType::CodeInner;
                    is_code = true;
                }
                LineType::CodeInner => {
                    is_code = true;
                }
            }
        }
        (line_type, code_syntax, is_code)
    }

    fn highlight_line(&self, line: &str, syntax: &SyntaxReference, is_code: bool) -> String {
        let ws: String = line.chars().take_while(|c| c.is_whitespace()).collect();
        let trimmed_line: &str = &line[ws.len()..];
        let mut line_highlighted = None;
        if let Some(theme) = &self.options.theme {
            let mut highlighter = HighlightLines::new(syntax, theme);
            if let Ok(ranges) = highlighter.highlight_line(trimmed_line, &self.syntax_set) {
                line_highlighted = Some(format!(
                    "{ws}{}",
                    as_terminal_escaped(&ranges, self.options.truecolor)
                ))
            }
        }
        let line = line_highlighted.unwrap_or_else(|| line.into());
        self.wrap_line(line, is_code)
    }

    fn highlight_code_line(&self, line: &str, code_syntax: &Option<SyntaxReference>) -> String {
        if let Some(syntax) = code_syntax {
            self.highlight_line(line, syntax, true)
        } else {
            let line = match self.code_color {
                Some(color) => line.with(color).to_string(),
                None => line.to_string(),
            };
            self.wrap_line(line, true)
        }
    }

    fn wrap_line(&self, line: String, is_code: bool) -> String {
        if let Some(width) = self.wrap_width {
            if is_code && !self.options.wrap_code {
                return line;
            }
            wrap(&line, width as usize)
        } else {
            line
        }
    }

    fn find_syntax(&self, lang: &str) -> Option<&SyntaxReference> {
        if let Some(new_lang) = LANG_MAPS.get(&lang.to_ascii_lowercase()) {
            self.syntax_set.find_syntax_by_name(new_lang)
        } else {
            self.syntax_set
                .find_syntax_by_token(lang)
                .or_else(|| self.syntax_set.find_syntax_by_extension(lang))
        }
    }
}

fn wrap(text: &str, width: usize) -> String {
    let indent: usize = text.chars().take_while(|c| *c == ' ').count();
    let wrap_options = textwrap::Options::new(width)
        .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit)
        .initial_indent(&text[0..indent]);
    textwrap::wrap(&text[indent..], wrap_options).join("\n")
}

fn preprocess_tables(text: &str, wrap_width: Option<usize>) -> String {
    let normalized = text.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let mut output = Vec::with_capacity(lines.len());
    let mut idx = 0;
    let mut in_fence = false;

    while idx < lines.len() {
        let line = lines[idx];
        // Toggle fence state on lines that start with ``` (with optional info string).
        // Use detect_code_block so 4+-space-indented backticks are not counted.
        if detect_code_block(line).is_some() {
            in_fence = !in_fence;
            output.push(line.to_string());
            idx += 1;
            continue;
        }
        if in_fence {
            // Inside a code fence: always emit raw line, never parse as table.
            output.push(line.to_string());
            idx += 1;
        } else if let Some((table, consumed)) = try_parse_table_block(&lines[idx..], wrap_width) {
            output.push(table);
            idx += consumed;
        } else {
            output.push(line.to_string());
            idx += 1;
        }
    }

    output.join("\n")
}

fn try_parse_table_block(lines: &[&str], wrap_width: Option<usize>) -> Option<(String, usize)> {
    let first_line = *lines.first()?;
    let mut block = vec![first_line];

    for next_line in lines.iter().skip(1).copied() {
        if next_line.trim().is_empty() {
            break;
        }
        if next_line.trim_start().starts_with('|') {
            block.push(next_line);
        } else {
            break;
        }
    }

    if block.len() < 2 {
        return None;
    }

    let consumed = block.len();
    let block_text = block.join("\n");
    let parser = Parser::new_ext(&block_text, Options::ENABLE_TABLES);
    let mut in_table = false;
    let mut in_head = false;
    let mut current_cell = String::new();
    let mut current_row = Vec::new();
    let mut headers = Vec::new();
    let mut rows = Vec::new();
    let mut alignments = Vec::new();

    for event in parser {
        match event {
            Event::Start(Tag::Table(found_alignments)) => {
                in_table = true;
                alignments = found_alignments;
            }
            Event::End(TagEnd::Table) => break,
            Event::Start(Tag::TableHead) => in_head = true,
            Event::End(TagEnd::TableHead) => {
                if !current_row.is_empty() {
                    headers = mem::take(&mut current_row);
                }
                in_head = false;
            }
            Event::Start(Tag::TableRow) => current_row.clear(),
            Event::End(TagEnd::TableRow) => {
                if in_head {
                    headers = mem::take(&mut current_row);
                } else {
                    rows.push(mem::take(&mut current_row));
                }
            }
            Event::Start(Tag::TableCell) => current_cell.clear(),
            Event::End(TagEnd::TableCell) => current_row.push(normalize_table_cell(&current_cell)),
            Event::Text(value) | Event::Code(value) => current_cell.push_str(&value),
            Event::SoftBreak | Event::HardBreak => current_cell.push(' '),
            _ => {}
        }
    }

    if !in_table || headers.is_empty() {
        return None;
    }

    Some((
        format_table(&headers, &rows, &alignments, wrap_width),
        consumed,
    ))
}

fn normalize_table_cell(cell: &str) -> String {
    cell.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn format_table(
    headers: &[String],
    rows: &[Vec<String>],
    alignments: &[Alignment],
    wrap_width: Option<usize>,
) -> String {
    let col_count = iter::once(headers.len())
        .chain(rows.iter().map(Vec::len))
        .max()
        .unwrap_or_default();
    let mut widths = vec![0; col_count];

    for row in iter::once(headers).chain(rows.iter().map(Vec::as_slice)) {
        for (idx, cell) in row.iter().enumerate() {
            widths[idx] = widths[idx].max(cell.chars().count());
        }
    }

    if let Some(max_width) = wrap_width {
        clamp_table_width(&mut widths, max_width);
    }

    let header = format_table_row(headers, &widths, alignments);
    let separator = format_table_separator(&widths, alignments);
    let body = rows
        .iter()
        .map(|row| format_table_row(row, &widths, alignments))
        .collect::<Vec<_>>();

    iter::once(header)
        .chain(iter::once(separator))
        .chain(body)
        .collect::<Vec<_>>()
        .join("\n")
}

fn clamp_table_width(widths: &mut [usize], max_width: usize) {
    let minimum_total = widths.len().saturating_mul(4).saturating_add(1);
    if widths.is_empty() || minimum_total > max_width {
        return;
    }

    while table_total_width(widths) > max_width {
        let Some((idx, width)) = widths
            .iter_mut()
            .enumerate()
            .max_by_key(|(_, width)| **width)
        else {
            break;
        };
        if *width <= 1 {
            break;
        }
        *width -= 1;
        if idx >= widths.len() {
            break;
        }
    }
}

fn table_total_width(widths: &[usize]) -> usize {
    widths.iter().sum::<usize>() + widths.len().saturating_mul(3) + 1
}

fn format_table_row(cells: &[String], widths: &[usize], alignments: &[Alignment]) -> String {
    let mut row = String::from("|");

    for (idx, width) in widths.iter().copied().enumerate() {
        let cell = cells.get(idx).map_or("", String::as_str);
        let cell = truncate_cell(cell, width);
        let formatted = match alignments.get(idx).copied().unwrap_or(Alignment::None) {
            Alignment::Left => format!(" {:<width$} |", cell, width = width),
            Alignment::Center => {
                let padding = width.saturating_sub(cell.chars().count());
                let left = padding / 2;
                let right = padding - left;
                format!(" {}{}{} |", " ".repeat(left), cell, " ".repeat(right))
            }
            Alignment::Right => format!(" {:>width$} |", cell, width = width),
            Alignment::None => format!(" {:<width$} |", cell, width = width),
        };
        row.push_str(&formatted);
    }

    row
}

fn truncate_cell(cell: &str, width: usize) -> String {
    cell.chars().take(width).collect()
}

fn format_table_separator(widths: &[usize], alignments: &[Alignment]) -> String {
    let mut row = String::from("|");

    for (idx, width) in widths.iter().copied().enumerate() {
        let dash_count = width.max(1);
        let segment = match alignments.get(idx).copied().unwrap_or(Alignment::None) {
            Alignment::Left => format!(":{:-<width$}|", "", width = dash_count + 1),
            Alignment::Center => {
                if dash_count == 1 {
                    String::from("::|")
                } else {
                    format!(":{:-<width$}:|", "", width = dash_count)
                }
            }
            Alignment::Right => format!("{:-<width$}:|", "", width = dash_count + 1),
            Alignment::None => format!("{:-<width$}|", "", width = dash_count + 2),
        };
        row.push_str(&segment);
    }

    row
}

#[derive(Debug, Clone, Default)]
pub struct RenderOptions {
    pub theme: Option<Theme>,
    pub wrap: Option<String>,
    pub wrap_code: bool,
    pub truecolor: bool,
}

impl RenderOptions {
    pub fn new(
        theme: Option<Theme>,
        wrap: Option<String>,
        wrap_code: bool,
        truecolor: bool,
    ) -> Self {
        Self {
            theme,
            wrap,
            wrap_code,
            truecolor,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineType {
    Normal,
    CodeBegin,
    CodeInner,
    CodeEnd,
}

fn as_terminal_escaped(ranges: &[(Style, &str)], truecolor: bool) -> String {
    let mut output = String::new();
    for (style, text) in ranges {
        let fg = blend_fg_color(style.foreground, style.background);
        let mut text = text.with(convert_color(fg, truecolor));
        if style.font_style.contains(FontStyle::BOLD) {
            text = text.bold();
        }
        if style.font_style.contains(FontStyle::UNDERLINE) {
            text = text.underlined();
        }
        output.push_str(&text.to_string());
    }
    output
}

fn convert_color(c: SyntectColor, truecolor: bool) -> Color {
    if truecolor {
        Color::Rgb {
            r: c.r,
            g: c.g,
            b: c.b,
        }
    } else {
        let value = (c.r, c.g, c.b).to_ansi256();
        // lower contrast
        let value = match value {
            7 | 15 | 231 | 252..=255 => 252,
            _ => value,
        };
        Color::AnsiValue(value)
    }
}

fn blend_fg_color(fg: SyntectColor, bg: SyntectColor) -> SyntectColor {
    if fg.a == 0xff {
        return fg;
    }
    let ratio = u32::from(fg.a);
    let r = (u32::from(fg.r) * ratio + u32::from(bg.r) * (255 - ratio)) / 255;
    let g = (u32::from(fg.g) * ratio + u32::from(bg.g) * (255 - ratio)) / 255;
    let b = (u32::from(fg.b) * ratio + u32::from(bg.b) * (255 - ratio)) / 255;
    SyntectColor {
        r: u8::try_from(r).unwrap_or(u8::MAX),
        g: u8::try_from(g).unwrap_or(u8::MAX),
        b: u8::try_from(b).unwrap_or(u8::MAX),
        a: 255,
    }
}

fn detect_code_block(line: &str) -> Option<String> {
    // Per CommonMark spec, a fenced code block may be indented by 0–3 spaces.
    // Four or more spaces of indentation means this is NOT a code fence.
    let indent = line.chars().take_while(|c| *c == ' ').count();
    if indent >= 4 {
        return None;
    }
    let line = &line[indent..];
    if !line.starts_with("```") {
        return None;
    }
    let lang = line
        .chars()
        .skip(3)
        .take_while(|v| !v.is_whitespace())
        .collect();
    Some(lang)
}

fn get_code_color(theme: &Theme, truecolor: bool) -> Color {
    let scope = theme.scopes.iter().find(|v| {
        v.scope
            .selectors
            .iter()
            .any(|v| v.path.scopes.iter().any(|v| v.to_string() == "string"))
    });
    scope
        .and_then(|v| v.style.foreground)
        .map_or_else(|| Color::Yellow, |c| convert_color(c, truecolor))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = r#"
To unzip a file in Rust, you can use the `zip` crate. Here's an example code that shows how to unzip a file:

```rust
use std::fs::File;

fn unzip_file(path: &str, output_dir: &str) -> Result<(), Box<dyn std::error::Error>> {
    todo!()
}
```
"#;
    const TEXT_NO_WRAP_CODE: &str = r#"
To unzip a file in Rust, you can use the `zip` crate. Here's an example code
that shows how to unzip a file:

```rust
use std::fs::File;

fn unzip_file(path: &str, output_dir: &str) -> Result<(), Box<dyn std::error::Error>> {
    todo!()
}
```
"#;

    const TEXT_WRAP_ALL: &str = r#"
To unzip a file in Rust, you can use the `zip` crate. Here's an example code
that shows how to unzip a file:

```rust
use std::fs::File;

fn unzip_file(path: &str, output_dir: &str) -> Result<(), Box<dyn
std::error::Error>> {
    todo!()
}
```
"#;

    #[test]
    fn test_render() {
        let options = RenderOptions::default();
        let render = MarkdownRender::init(options).unwrap();
        assert!(render.find_syntax("csharp").is_some());
    }

    #[test]
    fn no_theme() {
        let options = RenderOptions::default();
        let mut render = MarkdownRender::init(options).unwrap();
        let output = render.render(TEXT);
        assert_eq!(TEXT, output);
    }

    #[test]
    fn no_wrap_code() {
        let options = RenderOptions::default();
        let mut render = MarkdownRender::init(options).unwrap();
        render.wrap_width = Some(80);
        let output = render.render(TEXT);
        assert_eq!(TEXT_NO_WRAP_CODE, output);
    }

    #[test]
    fn wrap_all() {
        let options = RenderOptions {
            wrap_code: true,
            ..Default::default()
        };
        let mut render = MarkdownRender::init(options).unwrap();
        render.wrap_width = Some(80);
        let output = render.render(TEXT);
        assert_eq!(TEXT_WRAP_ALL, output);
    }

    #[test]
    fn test_detect_code_block() {
        assert_eq!(detect_code_block("```rust"), Some("rust".into()));
        assert_eq!(detect_code_block("```c++"), Some("c++".into()));
        assert_eq!(detect_code_block("  ```rust"), Some("rust".into()));
        assert_eq!(detect_code_block("   ```rust"), Some("rust".into()));
        assert_eq!(detect_code_block("```"), Some("".into()));
        assert_eq!(detect_code_block("``rust"), None);
        // 4+ spaces of indentation must NOT be treated as a code fence (CommonMark spec).
        // This is the regression test for issue #403: bash command text containing
        // indented lines with triple backticks was incorrectly toggling code-block state.
        assert_eq!(detect_code_block("    ```"), None);
        assert_eq!(detect_code_block("    ```python"), None);
        assert_eq!(detect_code_block("        ```"), None);
    }

    #[test]
    fn gfm_table_renders_as_aligned_columns() {
        let input = "| Name  | Age |\n| ----- | --- |\n| Alice | 30  |\n| Bob   | 25  |\n";
        let options = RenderOptions::default();
        let mut render = MarkdownRender::init(options).unwrap();
        let output = render.render(input);
        assert!(output.contains("Name"), "header missing: {output}");
        assert!(output.contains("Alice"), "row missing: {output}");
        assert!(
            !output.contains("| ----- | --- |"),
            "raw separator leaked: {output}"
        );
        assert!(output.contains('|'), "column separators missing: {output}");
        assert!(
            output.contains("|-------|-----|"),
            "formatted separator missing: {output}"
        );
    }

    /// Regression test for issue #403: bash output containing indented triple-backtick
    /// sequences (e.g. from a Python heredoc) must not toggle code-block rendering state.
    #[test]
    fn indented_backticks_not_treated_as_code_fence() {
        // Simulate the bash command display that triggered the bug: a multi-line Python
        // snippet where some lines happen to start with 4-space-indented triple backticks.
        let input = "Here is some text\n\
                     \n\
                     ```python\n\
                     for block in blocks:\n\
                         items = re.findall(r'```', block)\n\
                         print(items)\n\
                     ```\n\
                     \n\
                     After code.\n";

        let options = RenderOptions::default();
        let mut render = MarkdownRender::init(options).unwrap();
        let output = render.render(input);
        // The output must be identical to the input when there is no theme (no ANSI escapes).
        assert_eq!(output, input);
    }
}
