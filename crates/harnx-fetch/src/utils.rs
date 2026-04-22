use std::{cell::RefCell, rc::Rc};

use html_to_markdown::{markdown, TagHandler};

/// Convert HTML to Markdown. Inlined from `harnx::utils::html_to_md`.
pub(crate) fn html_to_md(html: &str) -> String {
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
        .unwrap_or_else(|_| html.to_string())
}

/// Format an anyhow error with its cause chain. Inlined from `harnx::utils::pretty_error`.
pub(crate) fn pretty_error(err: &anyhow::Error) -> String {
    let mut output = vec![];
    output.push(format!("Error: {err}"));
    let causes: Vec<_> = err.chain().skip(1).collect();
    let causes_len = causes.len();
    if causes_len > 0 {
        output.push("\nCaused by:".to_string());
        if causes_len == 1 {
            output.push(format!("    {}", indent_text(causes[0], 4).trim()));
        } else {
            for (i, cause) in causes.into_iter().enumerate() {
                output.push(format!("{i:5}: {}", indent_text(cause, 7).trim()));
            }
        }
    }
    output.join("\n")
}

fn indent_text<T: ToString>(s: T, size: usize) -> String {
    let indent_str = " ".repeat(size);
    s.to_string()
        .split('\n')
        .map(|line| format!("{indent_str}{line}"))
        .collect::<Vec<String>>()
        .join("\n")
}

/// Apply red ANSI colouring (if a terminal). Inlined from `harnx::utils::error_text`.
pub(crate) fn error_text(input: &str) -> String {
    color_text(input, nu_ansi_term::Color::Red)
}

/// Apply yellow ANSI colouring (if a terminal). Inlined from `harnx::utils::warning_text`.
pub(crate) fn warning_text(input: &str) -> String {
    color_text(input, nu_ansi_term::Color::Yellow)
}

fn color_text(input: &str, color: nu_ansi_term::Color) -> String {
    // Respect NO_COLOR / non-terminal; keep simple: always paint since
    // harnx-fetch has no access to the IS_STDOUT_TERMINAL / NO_COLOR statics
    // in harnx. Callers that don't want colour should strip ANSI themselves.
    nu_ansi_term::Style::new()
        .fg(color)
        .paint(input)
        .to_string()
}
