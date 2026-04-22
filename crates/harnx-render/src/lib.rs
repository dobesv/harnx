//! Terminal rendering (markdown + ANSI) for harnx. Moved from
//! `crates/harnx/src/render/` in Plan 42 (2026-04-22). See
//! `docs/superpowers/specs/2026-04-21-frontend-crate-splits-design.md`.

mod markdown;

pub use self::markdown::{MarkdownRender, RenderOptions};

pub fn render_error(err: anyhow::Error) {
    eprintln!("{}", error_text(&pretty_error(&err)));
}

/// Built-in Monokai Extended theme bytes — bincode-legacy-encoded
/// `syntect::highlighting::Theme`. Use [`load_builtin_theme`] to decode.
const DARK_THEME_BYTES: &[u8] = include_bytes!("../assets/monokai-extended.theme.bin");
const LIGHT_THEME_BYTES: &[u8] = include_bytes!("../assets/monokai-extended-light.theme.bin");

/// Decode the built-in Monokai Extended theme (light or dark variant).
/// Pairs with [`MarkdownRender`]'s `theme: Option<Theme>` field — callers
/// that want the default harnx look-and-feel decode this once at startup
/// and pass it into `RenderOptions`.
pub fn load_builtin_theme(light: bool) -> anyhow::Result<syntect::highlighting::Theme> {
    use anyhow::Context;
    let (bytes, label) = if light {
        (LIGHT_THEME_BYTES, "light")
    } else {
        (DARK_THEME_BYTES, "dark")
    };
    decode_bin(bytes).with_context(|| format!("Invalid builtin {label} theme"))
}

/// Inlined from `harnx::utils::decode_bin` — bincode-legacy decode for
/// the bundled `syntaxes.bin` and any other embedded binary assets.
/// Duplicated here to keep harnx-core free of the bincode dep.
pub(crate) fn decode_bin<T: serde::de::DeserializeOwned>(data: &[u8]) -> anyhow::Result<T> {
    let (v, _) = bincode::serde::decode_from_slice(data, bincode::config::legacy())?;
    Ok(v)
}

/// Inlined from `harnx::utils::error_text`. Respects `NO_COLOR` env
/// var and detects whether stdout is a terminal so callers that
/// relied on the original helper's suppress-when-not-a-tty behavior
/// continue to see plain-text output in non-tty contexts.
fn error_text(input: &str) -> String {
    if no_color() {
        return input.to_string();
    }
    nu_ansi_term::Style::new()
        .fg(nu_ansi_term::Color::Red)
        .paint(input)
        .to_string()
}

fn no_color() -> bool {
    use std::io::IsTerminal;
    static NO_COLOR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *NO_COLOR.get_or_init(|| {
        let env_flag = std::env::var("NO_COLOR")
            .ok()
            .map(|v| matches!(v.as_str(), "1" | "true"))
            .unwrap_or(false);
        env_flag || !std::io::stdout().is_terminal()
    })
}

/// Inlined from `harnx::utils::pretty_error`. Formats an
/// `anyhow::Error` with its cause chain in the same layout the
/// original harnx helper used.
fn pretty_error(err: &anyhow::Error) -> String {
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

/// Helper for `pretty_error`. Also inlined from `harnx::utils::indent_text`.
fn indent_text<T: ToString>(s: T, size: usize) -> String {
    let indent_str = " ".repeat(size);
    s.to_string()
        .split('\n')
        .map(|line| format!("{indent_str}{line}"))
        .collect::<Vec<String>>()
        .join("\n")
}
