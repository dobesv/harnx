use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use harnx_core::hooks::{HookConfig, HooksConfig};
use harnx_hooks::executor::HARNX_PACKAGE_DIR_ENV;

pub fn parse_args() -> Result<(HooksConfig, Vec<String>)> {
    let package_dir = std::env::var_os(HARNX_PACKAGE_DIR_ENV).map(PathBuf::from);
    parse_args_from_with_package_dir(std::env::args().skip(1), package_dir)
}

fn collect_hook_tokens<I>(iter: &mut I, flag: &str) -> Result<Vec<String>>
where
    I: Iterator<Item = String>,
{
    let mut tokens = Vec::new();
    for token in iter.by_ref() {
        if token == ";" || token == "\\;" {
            return Ok(tokens);
        }
        tokens.push(token);
    }
    bail!("unterminated {flag} (missing ';')")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn build_hook_command(command: &str, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(shell_quote(command));
    for arg in args {
        parts.push(shell_quote(arg));
    }
    parts.join(" ")
}

fn parse_args_from_with_package_dir<I>(
    args: I,
    package_dir: Option<PathBuf>,
) -> Result<(HooksConfig, Vec<String>)>
where
    I: IntoIterator<Item = String>,
{
    let mut hooks = Vec::new();
    let mut iter = args.into_iter();

    loop {
        let Some(arg) = iter.next() else {
            bail!("missing `--` separator before child command");
        };

        match arg.as_str() {
            "--" => {
                let child_cmd_args: Vec<String> = iter.collect();
                if child_cmd_args.is_empty() {
                    bail!("empty child command after `--`");
                }
                return Ok((
                    HooksConfig {
                        max_resume: None,
                        entries: hooks,
                    },
                    child_cmd_args,
                ));
            }
            "--pre-tool-use" => hooks.push(parse_hook_flag(
                "PreToolUse",
                &arg,
                &mut iter,
                package_dir.as_deref(),
            )?),
            "--post-tool-use" => hooks.push(parse_hook_flag(
                "PostToolUse",
                &arg,
                &mut iter,
                package_dir.as_deref(),
            )?),
            "--post-tool-use-failure" => hooks.push(parse_hook_flag(
                "PostToolUseFailure",
                &arg,
                &mut iter,
                package_dir.as_deref(),
            )?),
            other => bail!("unexpected argument before `--`: {other}"),
        }
    }
}

fn parse_hook_flag<I>(
    event: &str,
    flag: &str,
    iter: &mut I,
    package_dir: Option<&Path>,
) -> Result<HookConfig>
where
    I: Iterator<Item = String>,
{
    let hook_tokens = collect_hook_tokens(iter, flag)?;
    parse_hook_tokens(event, flag, hook_tokens, package_dir)
}

fn parse_hook_tokens(
    event: &str,
    flag: &str,
    tokens: Vec<String>,
    package_dir: Option<&Path>,
) -> Result<HookConfig> {
    let mut iter = tokens.into_iter();

    let hook_type = iter
        .next()
        .with_context(|| format!("{flag} requires a TYPE before command"))?;

    let mut async_hook = None;
    let mut matcher = None;
    let mut command = None;
    let mut args = Vec::new();

    while let Some(token) = iter.next() {
        match token.as_str() {
            "--async" if command.is_none() => {
                async_hook = Some(true);
            }
            "--matcher" if command.is_none() => {
                let regex = iter
                    .next()
                    .ok_or_else(|| anyhow!("{flag} --matcher requires a REGEX"))?;
                matcher = Some(regex);
            }
            _ => {
                command = Some(token);
                args.extend(iter);
                break;
            }
        }
    }

    let command =
        command.ok_or_else(|| anyhow!("{flag} requires a command after TYPE and options"))?;
    let command = build_hook_command(&command, &args);

    Ok(HookConfig {
        event: event.to_string(),
        matcher,
        command,
        timeout: Some(30),
        status_message: None,
        async_hook,
        hook_type,
        package_dir: package_dir.map(Path::to_path_buf),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args_from<I>(args: I) -> Result<(HooksConfig, Vec<String>)>
    where
        I: IntoIterator<Item = String>,
    {
        parse_args_from_with_package_dir(args, None)
    }

    fn one_hook_args() -> Vec<String> {
        vec![
            "--pre-tool-use".to_string(),
            "claude-command".to_string(),
            "my-hook-script".to_string(),
            ";".to_string(),
            "--".to_string(),
            "child-bin".to_string(),
        ]
    }

    #[test]
    fn inherited_package_dir_is_applied_to_every_hook() {
        let package_dir = PathBuf::from("/tmp/harnx-hooks-proxy-package");
        let mut args = one_hook_args();
        args.splice(
            4..4,
            [
                "--post-tool-use".to_string(),
                "claude-command".to_string(),
                "post-hook-script".to_string(),
                ";".to_string(),
            ],
        );

        let (hooks, _) = parse_args_from_with_package_dir(args, Some(package_dir.clone()))
            .expect("parse succeeds");

        assert_eq!(hooks.entries.len(), 2);
        assert!(hooks
            .entries
            .iter()
            .all(|hook| hook.package_dir.as_ref() == Some(&package_dir)));
    }

    #[test]
    fn package_dir_remains_none_when_environment_is_unset() {
        let (hooks, _) =
            parse_args_from_with_package_dir(one_hook_args(), None).expect("parse succeeds");

        assert_eq!(hooks.entries[0].package_dir, None);
    }

    #[test]
    fn parses_single_hook_and_child_command() {
        let (hooks, child) = parse_args_from(vec![
            "--pre-tool-use".to_string(),
            "claude-command".to_string(),
            "my-hook-script".to_string(),
            ";".to_string(),
            "--".to_string(),
            "child-bin".to_string(),
            "--flag".to_string(),
        ])
        .expect("parse succeeds");

        assert_eq!(hooks.entries.len(), 1);
        let hook = &hooks.entries[0];
        assert_eq!(hook.event, "PreToolUse");
        assert_eq!(hook.hook_type, "claude-command");
        assert_eq!(hook.command, "'my-hook-script'");
        assert_eq!(child, vec!["child-bin", "--flag"]);
    }

    #[test]
    fn parses_hook_with_matcher_and_async_flags() {
        let (hooks, child) = parse_args_from(vec![
            "--pre-tool-use".to_string(),
            "claude-command".to_string(),
            "--async".to_string(),
            "--matcher".to_string(),
            "bash.*".to_string(),
            "my-script".to_string(),
            ";".to_string(),
            "--".to_string(),
            "child".to_string(),
        ])
        .expect("parse succeeds");

        assert_eq!(hooks.entries.len(), 1);
        let hook = &hooks.entries[0];
        assert_eq!(hook.event, "PreToolUse");
        assert_eq!(hook.matcher.as_deref(), Some("bash.*"));
        assert_eq!(hook.async_hook, Some(true));
        assert_eq!(hook.command, "'my-script'");
        assert_eq!(child, vec!["child"]);
    }

    #[test]
    fn accepts_escaped_semicolon_terminator() {
        let (hooks, child) = parse_args_from(vec![
            "--pre-tool-use".to_string(),
            "claude-command".to_string(),
            "my-script".to_string(),
            "\\;".to_string(),
            "--".to_string(),
            "child".to_string(),
        ])
        .expect("parse succeeds");

        assert_eq!(hooks.entries.len(), 1);
        assert_eq!(hooks.entries[0].command, "'my-script'");
        assert_eq!(child, vec!["child"]);
    }

    #[test]
    fn preserves_and_quotes_multiple_args() {
        let (hooks, _) = parse_args_from(vec![
            "--pre-tool-use".to_string(),
            "claude-command".to_string(),
            "hook script".to_string(),
            "arg with spaces".to_string(),
            "it's-fine".to_string(),
            ";".to_string(),
            "--".to_string(),
            "child".to_string(),
        ])
        .expect("parse succeeds");

        assert_eq!(
            hooks.entries[0].command,
            "'hook script' 'arg with spaces' 'it'\\''s-fine'"
        );
    }

    #[test]
    fn parses_multiple_hooks_of_different_events() {
        let (hooks, child) = parse_args_from(vec![
            "--pre-tool-use".to_string(),
            "claude-command".to_string(),
            "pre-script".to_string(),
            ";".to_string(),
            "--post-tool-use".to_string(),
            "claude-command".to_string(),
            "post-script".to_string(),
            ";".to_string(),
            "--post-tool-use-failure".to_string(),
            "claude-command".to_string(),
            "fail-script".to_string(),
            ";".to_string(),
            "--".to_string(),
            "child".to_string(),
        ])
        .expect("parse succeeds");

        assert_eq!(hooks.entries.len(), 3);
        assert_eq!(hooks.entries[0].event, "PreToolUse");
        assert_eq!(hooks.entries[0].command, "'pre-script'");
        assert_eq!(hooks.entries[1].event, "PostToolUse");
        assert_eq!(hooks.entries[1].command, "'post-script'");
        assert_eq!(hooks.entries[2].event, "PostToolUseFailure");
        assert_eq!(hooks.entries[2].command, "'fail-script'");
        assert_eq!(child, vec!["child"]);
    }

    #[test]
    fn error_on_unterminated_hook() {
        let result = parse_args_from(vec![
            "--pre-tool-use".to_string(),
            "claude-command".to_string(),
            "script".to_string(),
        ]);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "unterminated --pre-tool-use (missing ';')"
        );
    }

    #[test]
    fn error_on_missing_command() {
        let result = parse_args_from(vec![
            "--pre-tool-use".to_string(),
            "claude-command".to_string(),
            "--async".to_string(),
            "--matcher".to_string(),
            "bash.*".to_string(),
            ";".to_string(),
            "--".to_string(),
            "child".to_string(),
        ]);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "--pre-tool-use requires a command after TYPE and options"
        );
    }

    #[test]
    fn error_on_missing_separator() {
        let result = parse_args_from(vec![
            "--pre-tool-use".to_string(),
            "claude-command".to_string(),
            "script".to_string(),
            ";".to_string(),
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn error_on_empty_child_command() {
        let result = parse_args_from(vec![
            "--pre-tool-use".to_string(),
            "claude-command".to_string(),
            "script".to_string(),
            ";".to_string(),
            "--".to_string(),
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn error_on_unexpected_arg_before_separator() {
        let result = parse_args_from(vec![
            "--bogus".to_string(),
            "--pre-tool-use".to_string(),
            "claude-command".to_string(),
            "script".to_string(),
            ";".to_string(),
            "--".to_string(),
            "child".to_string(),
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn error_on_matcher_without_regex() {
        let result = parse_args_from(vec![
            "--pre-tool-use".to_string(),
            "claude-command".to_string(),
            "--matcher".to_string(),
            ";".to_string(),
            "--".to_string(),
            "child".to_string(),
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn option_like_tokens_after_command_are_literal_args() {
        let (hooks, child) = parse_args_from(vec![
            "--pre-tool-use".to_string(),
            "claude-command".to_string(),
            "my-cmd".to_string(),
            "--async".to_string(),
            ";".to_string(),
            "--".to_string(),
            "child".to_string(),
        ])
        .expect("parse succeeds");

        assert_eq!(hooks.entries.len(), 1);
        let hook = &hooks.entries[0];
        assert_eq!(hook.event, "PreToolUse");
        assert_eq!(hook.async_hook, None);
        assert!(hook.command.contains("--async"));
        assert_eq!(child, vec!["child"]);
    }
}
