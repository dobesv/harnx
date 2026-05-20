use anyhow::{anyhow, bail, Context, Result};
use harnx_core::hooks::{HookConfig, HooksConfig};

pub fn parse_args() -> Result<(HooksConfig, Vec<String>)> {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from<I>(args: I) -> Result<(HooksConfig, Vec<String>)>
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
            "--pre-tool-use" => hooks.push(parse_hook_flag("PreToolUse", &mut iter)?),
            "--post-tool-use" => hooks.push(parse_hook_flag("PostToolUse", &mut iter)?),
            "--post-tool-use-failure" => {
                hooks.push(parse_hook_flag("PostToolUseFailure", &mut iter)?)
            }
            other => bail!("unknown argument: {other}"),
        }
    }
}

fn parse_hook_flag<I>(event: &str, iter: &mut I) -> Result<HookConfig>
where
    I: Iterator<Item = String>,
{
    let value = iter
        .next()
        .with_context(|| format!("missing value for flag corresponding to {event}"))?;
    parse_hook_spec(event, &value)
}

fn parse_hook_spec(event: &str, spec: &str) -> Result<HookConfig> {
    let tokens: Vec<&str> = spec.split_whitespace().collect();
    if tokens.is_empty() {
        bail!("empty hook specification for {event}");
    }

    let hook_type = tokens[0].to_string();
    let mut async_hook = None;
    let mut matcher = None;
    let mut command_tokens = Vec::new();
    let mut index = 1;

    while index < tokens.len() {
        match tokens[index] {
            "--async" => {
                async_hook = Some(true);
                index += 1;
            }
            "--matcher" => {
                let regex = tokens
                    .get(index + 1)
                    .ok_or_else(|| anyhow!("missing regex after --matcher in {event}"))?;
                matcher = Some((*regex).to_string());
                index += 2;
            }
            _ => {
                command_tokens.extend(tokens[index..].iter().copied());
                break;
            }
        }
    }

    if command_tokens.is_empty() {
        bail!("missing hook command for {event}");
    }

    Ok(HookConfig {
        event: event.to_string(),
        matcher,
        command: command_tokens.join(" "),
        timeout: Some(30),
        status_message: None,
        async_hook,
        hook_type,
    })
}

#[cfg(test)]
mod tests {
    use super::parse_args_from;

    #[test]
    fn parses_basic_hook_and_child_command() {
        let (hooks, child) = parse_args_from(vec![
            "--pre-tool-use".to_string(),
            "claude-command my-hook-script".to_string(),
            "--".to_string(),
            "child-bin".to_string(),
            "--flag".to_string(),
        ])
        .expect("parse succeeds");

        assert_eq!(hooks.entries.len(), 1);
        let hook = &hooks.entries[0];
        assert_eq!(hook.event, "PreToolUse");
        assert_eq!(hook.hook_type, "claude-command");
        assert_eq!(hook.command, "my-hook-script");
        assert_eq!(child, vec!["child-bin", "--flag"]);
    }

    #[test]
    fn parses_hook_with_matcher_and_async_flags() {
        let (hooks, child) = parse_args_from(vec![
            "--pre-tool-use".to_string(),
            "claude-command --async --matcher bash.* my-script".to_string(),
            "--".to_string(),
            "child".to_string(),
        ])
        .expect("parse succeeds");

        assert_eq!(hooks.entries.len(), 1);
        let hook = &hooks.entries[0];
        assert_eq!(hook.event, "PreToolUse");
        assert_eq!(hook.matcher.as_deref(), Some("bash.*"));
        assert_eq!(hook.async_hook, Some(true));
        assert_eq!(hook.command, "my-script");
        assert_eq!(child, vec!["child"]);
    }

    #[test]
    fn parses_multiple_hooks_of_different_events() {
        let (hooks, child) = parse_args_from(vec![
            "--pre-tool-use".to_string(),
            "claude-command pre-script".to_string(),
            "--post-tool-use".to_string(),
            "claude-command post-script".to_string(),
            "--post-tool-use-failure".to_string(),
            "claude-command fail-script".to_string(),
            "--".to_string(),
            "child".to_string(),
        ])
        .expect("parse succeeds");

        assert_eq!(hooks.entries.len(), 3);
        assert_eq!(hooks.entries[0].event, "PreToolUse");
        assert_eq!(hooks.entries[0].command, "pre-script");
        assert_eq!(hooks.entries[1].event, "PostToolUse");
        assert_eq!(hooks.entries[1].command, "post-script");
        assert_eq!(hooks.entries[2].event, "PostToolUseFailure");
        assert_eq!(hooks.entries[2].command, "fail-script");
        assert_eq!(child, vec!["child"]);
    }

    #[test]
    fn error_on_missing_separator() {
        let result = parse_args_from(vec![
            "--pre-tool-use".to_string(),
            "claude-command script".to_string(),
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn error_on_empty_child_command() {
        let result = parse_args_from(vec![
            "--pre-tool-use".to_string(),
            "claude-command script".to_string(),
            "--".to_string(),
        ]);
        assert!(result.is_err());
    }
}
