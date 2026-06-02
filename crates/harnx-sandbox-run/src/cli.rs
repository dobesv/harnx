//! CLI parsing with pre-parse hook extraction.
//!
//! The `--hook` flag uses a novel syntax that requires special parsing before clap:
//! `--hook <TYPE> <CMD> [ARGS...] \;`
//!
//! This module extracts hook definitions first, then passes remaining args to clap.

use std::ffi::OsString;
use std::path::PathBuf;

/// Parsed hook definition from `--hook TYPE CMD [ARGS...] ;` syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookDef {
    /// Hook type: "claude-command" or "claude-command-persistent".
    pub hook_type: String,
    /// Command to execute (first token after type).
    pub command: String,
    /// Arguments to pass to the command.
    pub args: Vec<String>,
}

/// Consume tokens from `iter` until `;` or `\;` and return them.
///
/// # Errors
///
/// Returns an error if the iterator is exhausted without seeing a terminator.
fn collect_hook_tokens(iter: &mut impl Iterator<Item = String>) -> anyhow::Result<Vec<String>> {
    let mut tokens = Vec::new();
    for token in iter.by_ref() {
        if token == ";" || token == "\\;" {
            return Ok(tokens);
        }
        tokens.push(token);
    }
    anyhow::bail!("harnx-sandbox-run: unterminated --hook (missing ';')")
}

/// Pre-parse `--hook` groups from raw arguments before clap.
///
/// Walks tokens; on `--hook` collects until `;` or `\;`. Returns the extracted
/// hook definitions and remaining arguments for clap.
///
/// # Errors
///
/// Returns an error if:
/// - A `--hook` is not terminated with `;` or `\;`
/// - A `--hook` has fewer than 2 tokens (need type + command)
pub fn pre_parse_hooks(raw: Vec<String>) -> anyhow::Result<(Vec<HookDef>, Vec<String>)> {
    let mut hooks = Vec::new();
    let mut remaining = Vec::new();
    let mut tokens = raw.into_iter().peekable();

    while let Some(token) = tokens.next() {
        if token == "--" {
            remaining.push(token);
            remaining.extend(tokens);
            break;
        } else if token == "--hook" {
            let hook_tokens = collect_hook_tokens(&mut tokens)?;

            if hook_tokens.len() < 2 {
                anyhow::bail!(
                    "harnx-sandbox-run: --hook requires at least TYPE and COMMAND (got {} tokens)",
                    hook_tokens.len()
                );
            }

            hooks.push(HookDef {
                hook_type: hook_tokens[0].clone(),
                command: hook_tokens[1].clone(),
                args: hook_tokens[2..].to_vec(),
            });
        } else {
            remaining.push(token);
        }
    }

    Ok((hooks, remaining))
}

/// Parsed CLI arguments for harnx-sandbox-run.
#[derive(Debug, clap::Parser)]
#[command(name = "harnx-sandbox-run")]
#[command(about = "Run commands inside the birdcage sandbox with hook support")]
pub struct Cli {
    /// Add sandbox read-only path (may be repeated). Supports project-root pseudo-vars like $GIT_ROOT (see docs).
    #[arg(long, value_name = "path")]
    pub extra_read: Vec<PathBuf>,

    /// Add sandbox writable path (may be repeated). Supports project-root pseudo-vars like $GIT_ROOT (see docs).
    #[arg(long, value_name = "path")]
    pub extra_write: Vec<PathBuf>,

    /// Add sandbox execute path (may be repeated). Supports project-root pseudo-vars like $GIT_ROOT (see docs).
    #[arg(long, value_name = "path")]
    pub extra_exec: Vec<PathBuf>,

    /// Add sandbox read/write/exec path (may be repeated). Supports project-root pseudo-vars like $GIT_ROOT (see docs).
    #[arg(long, value_name = "path")]
    pub extra_rwx: Vec<PathBuf>,

    /// Set environment variable (VAR=VALUE); if VALUE omitted, inherit from host.
    #[arg(long = "env", value_name = "VAR[=VALUE]")]
    pub env_vars: Vec<String>,

    /// Disable network access.
    #[arg(long)]
    pub no_network: bool,

    /// Working directory for the command.
    #[arg(long)]
    pub working_dir: Option<PathBuf>,

    /// Skip default whitelist (system paths, home paths, env-relative paths).
    #[arg(long)]
    pub no_defaults: bool,

    /// Command to run (required, must come after `--` or at end).
    #[arg(last = true, required = true)]
    pub command: Vec<OsString>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_hook_with_args() {
        let raw = vec![
            "--hook".to_string(),
            "claude-command".to_string(),
            "echo".to_string(),
            "hello".to_string(),
            ";".to_string(),
            "--".to_string(),
            "bash".to_string(),
        ];
        let (hooks, remaining) = pre_parse_hooks(raw).expect("parse");
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].hook_type, "claude-command");
        assert_eq!(hooks[0].command, "echo");
        assert_eq!(hooks[0].args, vec!["hello"]);
        assert_eq!(remaining, vec!["--", "bash"]);
    }

    #[test]
    fn extracts_hook_without_args() {
        let raw = vec![
            "--hook".to_string(),
            "claude-command".to_string(),
            "/bin/true".to_string(),
            ";".to_string(),
            "--".to_string(),
            "ls".to_string(),
        ];
        let (hooks, remaining) = pre_parse_hooks(raw).expect("parse");
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].command, "/bin/true");
        assert!(hooks[0].args.is_empty());
        assert_eq!(remaining, vec!["--", "ls"]);
    }

    #[test]
    fn accepts_escaped_semicolon() {
        let raw = vec![
            "--hook".to_string(),
            "claude-command".to_string(),
            "cat".to_string(),
            "\\;".to_string(),
            "--".to_string(),
            "ls".to_string(),
        ];
        let (hooks, _) = pre_parse_hooks(raw).expect("parse");
        assert_eq!(hooks.len(), 1);
    }

    #[test]
    fn rejects_unterminated_hook() {
        let raw = vec![
            "--hook".to_string(),
            "claude-command".to_string(),
            "echo".to_string(),
        ];
        let result = pre_parse_hooks(raw);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_hook_missing_command() {
        let raw = vec![
            "--hook".to_string(),
            "claude-command".to_string(),
            ";".to_string(),
        ];
        let result = pre_parse_hooks(raw);
        assert!(result.is_err());
    }

    #[test]
    fn extracts_multiple_hooks() {
        let raw = vec![
            "--hook".to_string(),
            "claude-command".to_string(),
            "a".to_string(),
            ";".to_string(),
            "--hook".to_string(),
            "claude-command-persistent".to_string(),
            "b".to_string(),
            ";".to_string(),
            "--".to_string(),
            "cmd".to_string(),
        ];
        let (hooks, _) = pre_parse_hooks(raw).expect("parse");
        assert_eq!(hooks.len(), 2);
        assert_eq!(hooks[0].command, "a");
        assert_eq!(hooks[1].command, "b");
    }

    #[test]
    fn handles_no_hooks() {
        let raw = vec!["--".to_string(), "ls".to_string(), "-la".to_string()];
        let (hooks, remaining) = pre_parse_hooks(raw).expect("parse");
        assert!(hooks.is_empty());
        assert_eq!(remaining, vec!["--", "ls", "-la"]);
    }

    #[test]
    fn stops_hook_scanning_after_separator() {
        let raw = vec![
            "--".to_string(),
            "my-tool".to_string(),
            "--hook".to_string(),
            "something".to_string(),
        ];
        let (hooks, remaining) = pre_parse_hooks(raw).expect("parse");
        assert!(hooks.is_empty());
        assert_eq!(remaining, vec!["--", "my-tool", "--hook", "something"]);
    }
}
