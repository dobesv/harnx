mod clipboard;
mod command;
pub mod session_name;
pub mod terminal_session;

pub use self::clipboard::set_text;
pub use self::command::*;
pub use self::terminal_session::terminal_session_id;
pub use harnx_core::abort::{
    create_abort_signal, wait_abort_signal, AbortSignal, AbortSignalInner,
};
pub use harnx_core::crypto::{base64_encode, sha256};

/// Emit a user-facing Info notice through the process-wide `AgentEventSink`,
/// falling back to stdout if no sink is installed (e.g. unit tests).
pub fn emit_info(msg: String) {
    let event =
        harnx_core::event::AgentEvent::Notice(harnx_core::event::NoticeEvent::Info(msg.clone()));
    if !harnx_core::sink::emit_agent_event(event) {
        println!("{msg}");
    }
}

/// Emit a user-facing Warning notice, falling back to stderr if no sink is
/// installed.
pub fn emit_warning(msg: String) {
    let event =
        harnx_core::event::AgentEvent::Notice(harnx_core::event::NoticeEvent::Warning(msg.clone()));
    if !harnx_core::sink::emit_agent_event(event) {
        eprintln!("{}", warning_text(&msg));
    }
}
pub use harnx_core::path::{
    expand_glob_paths, get_patch_extension, list_file_names, resolve_home_dir, safe_join_path,
    to_absolute_path,
};
pub use harnx_core::text::strip_think_tag;
pub use harnx_fetch::loader::*;
pub use harnx_fetch::request::*;
pub use harnx_spinner::*;

use anyhow::Result;
use fuzzy_matcher::{skim::SkimMatcherV2, FuzzyMatcher};
use is_terminal::IsTerminal;
use std::sync::LazyLock;
use std::{env, path::PathBuf, process};

pub static IS_STDOUT_TERMINAL: LazyLock<bool> = LazyLock::new(|| std::io::stdout().is_terminal());
pub static NO_COLOR: LazyLock<bool> = LazyLock::new(|| {
    env::var("NO_COLOR")
        .ok()
        .and_then(|v| parse_bool(&v))
        .unwrap_or_default()
        || !*IS_STDOUT_TERMINAL
});

pub fn now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// Render a `serde_json::Value` as a compact YAML block with trailing
/// whitespace trimmed. Falls back to the Value's Display form if YAML
/// serialization fails. Used for human-readable tool-call argument
/// blocks in transcripts and terminal fallback output.
pub fn pretty_yaml_block(value: &serde_json::Value) -> String {
    serde_yaml::to_string(value)
        .map(|s| s.trim_end().to_string())
        .unwrap_or_else(|_| value.to_string())
}

/// Format a tool's `Completed` event for terminal display.
///
/// Prefers `markdown` (set by harnx-runtime when an MCP `result_template`
/// matched the call) over the raw `output`. When `markdown` is `None` or
/// renders to whitespace, falls back to `extract_user_display_text` →
/// string passthrough → YAML, then truncates to fit the terminal when
/// stdout is one. Both the TUI sink and the CLI sink call this so they
/// render identically.
pub fn render_tool_result_text(output: &serde_json::Value, markdown: Option<&str>) -> String {
    use harnx_core::safety::{truncate_output, TruncateOpts};
    use harnx_core::tool::extract_user_display_text;

    let mut opts = TruncateOpts::default();
    let marker = " [...] ";
    // Without a terminal to fit, the safer `TruncateOpts::default()`
    // (head_lines=25, line_head_bytes=500) stays in place.
    if let Some((cols, rows)) = stdout_terminal_size() {
        opts.head_lines = 5.max((rows / 2) as usize);
        opts.tail_lines = 0;
        opts.line_head_bytes = (cols as usize).saturating_sub(3 + marker.len());
        opts.line_tail_bytes = 0;
        opts.marker = Some(marker.to_string());
    }
    let text = match markdown.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => t.to_string(),
        None => extract_user_display_text(output).unwrap_or_else(|| match output {
            serde_json::Value::String(s) => s.clone(),
            _ => pretty_yaml_block(output),
        }),
    };
    truncate_output(&text, &opts)
}

/// The size of the terminal stdout writes to, or `None` when stdout is not a
/// terminal. crossterm reports a size even then: the controlling terminal's,
/// or `tput`'s default for `TERM` (80x24) when there is none, so piped output
/// and test results would depend on who ran them. A (0, 0) size also counts
/// as none: `(cols as usize).saturating_sub(...)` would yield 0 and truncate
/// every line to nothing.
fn stdout_terminal_size() -> Option<(u16, u16)> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    crossterm::terminal::size()
        .ok()
        .filter(|&(cols, rows)| cols > 0 && rows > 0)
}

pub use harnx_core::config_paths::get_env_name;

pub fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}

pub fn fuzzy_filter<T, F>(values: Vec<T>, get: F, pattern: &str) -> Vec<T>
where
    F: Fn(&T) -> &str,
{
    let matcher = SkimMatcherV2::default();
    let mut list: Vec<(T, i64)> = values
        .into_iter()
        .filter_map(|v| {
            let score = matcher.fuzzy_match(get(&v), pattern)?;
            Some((v, score))
        })
        .collect();
    list.sort_unstable_by_key(|b| std::cmp::Reverse(b.1));
    list.into_iter().map(|(v, _)| v).collect()
}

pub fn warning_text(input: &str) -> String {
    color_text(input, nu_ansi_term::Color::Yellow)
}

pub fn color_text(input: &str, color: nu_ansi_term::Color) -> String {
    if *NO_COLOR {
        return input.to_string();
    }
    nu_ansi_term::Style::new()
        .fg(color)
        .paint(input)
        .to_string()
}

pub fn dimmed_text(input: &str) -> String {
    if *NO_COLOR {
        return input.to_string();
    }
    nu_ansi_term::Style::new().dimmed().paint(input).to_string()
}

pub fn multiline_text(input: &str) -> String {
    input
        .split('\n')
        .enumerate()
        .map(|(i, v)| {
            if i == 0 {
                v.to_string()
            } else {
                format!(".. {v}")
            }
        })
        .collect::<Vec<String>>()
        .join("\n")
}

pub fn temp_file(prefix: &str, suffix: &str) -> PathBuf {
    env::temp_dir().join(format!(
        "{}-{}{prefix}{}{suffix}",
        env!("CARGO_CRATE_NAME").to_lowercase(),
        process::id(),
        uuid::Uuid::new_v4()
    ))
}

pub fn is_url(path: &str) -> bool {
    path.starts_with("http://") || path.starts_with("https://")
}

/// Also inlined in `harnx_render::decode_bin` — keep in sync.
pub fn decode_bin<T: serde::de::DeserializeOwned>(data: &[u8]) -> Result<T> {
    let (v, _) = bincode::serde::decode_from_slice(data, bincode::config::legacy())?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests, like piped output, have no terminal on stdout. When `TERM`
    /// names a terminal type there, crossterm still reports `tput`'s 80
    /// columns, which used to cut tool results short.
    #[test]
    fn tool_results_fit_no_terminal_when_stdout_is_not_one() {
        // Sets TERM for the whole process.
        harnx_core::require_nextest();
        if std::io::stdout().is_terminal() {
            eprintln!("skipping: stdout is a terminal, as with --no-capture");
            return;
        }
        std::env::set_var("TERM", "xterm-256color");
        let line = "x".repeat(200);
        let rendered = render_tool_result_text(&serde_json::Value::String(line.clone()), None);
        assert_eq!(rendered, line);
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn test_safe_join_path() {
        assert_eq!(
            safe_join_path("/home/user/dir1", "files/file1"),
            Some(PathBuf::from("/home/user/dir1/files/file1"))
        );
        assert!(safe_join_path("/home/user/dir1", "/files/file1").is_none());
        assert!(safe_join_path("/home/user/dir1", "../file1").is_none());
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn test_safe_join_path() {
        assert_eq!(
            safe_join_path("C:\\Users\\user\\dir1", "files/file1"),
            Some(PathBuf::from("C:\\Users\\user\\dir1\\files\\file1"))
        );
        assert!(safe_join_path("C:\\Users\\user\\dir1", "/files/file1").is_none());
        assert!(safe_join_path("C:\\Users\\user\\dir1", "../file1").is_none());
    }
}
