use std::collections::HashMap;
use std::process::Command;

/// Expand `$VAR` and `${VAR}` patterns in a session name template.
///
/// Special computed variables:
/// - `$GIT_BRANCH` → current git branch (via `git rev-parse --abbrev-ref HEAD`)
/// - `$GIT_PATH`   → git-relative path of cwd (via `git rev-parse --show-prefix`, trailing `/` stripped)
///
/// Additional context variables can be supplied via `extra_vars` (e.g. `AGENT_NAME`).
///
/// All other variable names are resolved from environment variables.
/// Unresolved variables (env not set, git command fails) expand to empty string.
pub fn expand_session_variables(template: &str) -> String {
    expand_session_variables_with(template, &HashMap::new())
}

/// Like [`expand_session_variables`] but accepts extra key-value pairs that take
/// precedence over environment variables (but not over the built-in computed
/// variables `GIT_BRANCH` and `GIT_PATH`).
pub fn expand_session_variables_with(
    template: &str,
    extra_vars: &HashMap<&str, &str>,
) -> String {
    let mut result = String::with_capacity(template.len());
    let chars: Vec<char> = template.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if chars[i] == '$' && i + 1 < len {
            if chars[i + 1] == '{' {
                // ${VAR} syntax
                if let Some(end) = chars[i + 2..].iter().position(|&c| c == '}') {
                    let var_name: String = chars[i + 2..i + 2 + end].iter().collect();
                    result.push_str(&resolve_variable(&var_name, extra_vars));
                    i = i + 2 + end + 1; // skip past '}'
                } else {
                    // No closing brace — emit literal
                    result.push('$');
                    i += 1;
                }
            } else if chars[i + 1].is_ascii_alphanumeric() || chars[i + 1] == '_' {
                // $VAR syntax — collect alphanumeric + underscore chars
                let start = i + 1;
                let mut end = start;
                while end < len && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
                    end += 1;
                }
                let var_name: String = chars[start..end].iter().collect();
                result.push_str(&resolve_variable(&var_name, extra_vars));
                i = end;
            } else {
                // $ followed by something that's not a var name — emit literal
                result.push('$');
                i += 1;
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }

    result
}

/// Sanitize a session name for safe use as a filename / session identifier.
///
/// - `/` → `-`
/// - space → `_`
/// - Strip leading/trailing `-` and `_` characters
pub fn sanitize_session_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| match c {
            '/' => '-',
            ' ' => '_',
            other => other,
        })
        .collect();
    sanitized.trim_matches(|c: char| c == '-' || c == '_').to_string()
}

fn resolve_variable(name: &str, extra_vars: &HashMap<&str, &str>) -> String {
    match name {
        "GIT_BRANCH" => git_branch(),
        "GIT_PATH" => git_path(),
        _ => {
            if let Some(val) = extra_vars.get(name) {
                val.to_string()
            } else {
                std::env::var(name).unwrap_or_default()
            }
        }
    }
}

fn git_branch() -> String {
    Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn git_path() -> String {
    Command::new("git")
        .args(["rev-parse", "--show-prefix"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            s.trim_end_matches('/').to_string()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_no_variables() {
        assert_eq!(expand_session_variables("plain-name"), "plain-name");
    }

    #[test]
    fn test_expand_env_var() {
        std::env::set_var("HARNX_TEST_SESSION_VAR", "hello");
        assert_eq!(
            expand_session_variables("prefix-$HARNX_TEST_SESSION_VAR-suffix"),
            "prefix-hello-suffix"
        );
        std::env::remove_var("HARNX_TEST_SESSION_VAR");
    }

    #[test]
    fn test_expand_env_var_braced() {
        std::env::set_var("HARNX_TEST_BRACE_VAR", "world");
        assert_eq!(
            expand_session_variables("prefix-${HARNX_TEST_BRACE_VAR}-suffix"),
            "prefix-world-suffix"
        );
        std::env::remove_var("HARNX_TEST_BRACE_VAR");
    }

    #[test]
    fn test_expand_unset_env_var() {
        // Ensure the var doesn't exist
        std::env::remove_var("HARNX_NONEXISTENT_VAR_12345");
        assert_eq!(
            expand_session_variables("prefix-$HARNX_NONEXISTENT_VAR_12345-suffix"),
            "prefix--suffix"
        );
    }

    #[test]
    fn test_expand_git_branch() {
        // This test will work in a git repo — expand to something non-empty
        // Outside a git repo, it expands to empty string — both are valid
        let result = expand_session_variables("proj-$GIT_BRANCH");
        assert!(result.starts_with("proj-"));
    }

    #[test]
    fn test_expand_git_path() {
        let result = expand_session_variables("proj-$GIT_PATH");
        assert!(result.starts_with("proj-"));
    }

    #[test]
    fn test_expand_multiple_vars() {
        std::env::set_var("HARNX_TEST_A", "aaa");
        std::env::set_var("HARNX_TEST_B", "bbb");
        assert_eq!(
            expand_session_variables("$HARNX_TEST_A-$HARNX_TEST_B"),
            "aaa-bbb"
        );
        std::env::remove_var("HARNX_TEST_A");
        std::env::remove_var("HARNX_TEST_B");
    }

    #[test]
    fn test_expand_dollar_not_followed_by_var() {
        assert_eq!(expand_session_variables("price$"), "price$");
        assert_eq!(expand_session_variables("$"), "$");
    }

    #[test]
    fn test_expand_unclosed_brace() {
        assert_eq!(expand_session_variables("${UNCLOSED"), "${UNCLOSED");
    }

    #[test]
    fn test_expand_empty_template() {
        assert_eq!(expand_session_variables(""), "");
    }

    #[test]
    fn test_sanitize_slashes() {
        assert_eq!(sanitize_session_name("feat/branch"), "feat-branch");
    }

    #[test]
    fn test_sanitize_spaces() {
        assert_eq!(sanitize_session_name("my session"), "my_session");
    }

    #[test]
    fn test_sanitize_leading_trailing() {
        assert_eq!(sanitize_session_name("-_hello_-"), "hello");
        assert_eq!(sanitize_session_name("---test___"), "test");
    }

    #[test]
    fn test_sanitize_combined() {
        assert_eq!(sanitize_session_name("/feat/my branch/"), "feat-my_branch");
    }

    #[test]
    fn test_sanitize_empty() {
        assert_eq!(sanitize_session_name(""), "");
        assert_eq!(sanitize_session_name("---"), "");
    }

    #[test]
    fn test_sanitize_preserves_normal() {
        assert_eq!(sanitize_session_name("my-session_name"), "my-session_name");
    }

    #[test]
    fn test_expand_and_sanitize_integration() {
        std::env::set_var("HARNX_TEST_INTEG", "feat/cool-thing");
        let expanded = expand_session_variables("proj-$HARNX_TEST_INTEG");
        let sanitized = sanitize_session_name(&expanded);
        assert_eq!(sanitized, "proj-feat-cool-thing");
        std::env::remove_var("HARNX_TEST_INTEG");
    }

    #[test]
    fn test_expand_with_extra_vars() {
        let extra = std::collections::HashMap::from([("AGENT_NAME", "coder")]);
        assert_eq!(
            expand_session_variables_with("$AGENT_NAME-session", &extra),
            "coder-session"
        );
    }

    #[test]
    fn test_expand_with_extra_vars_braced() {
        let extra = std::collections::HashMap::from([("AGENT_NAME", "my-agent")]);
        assert_eq!(
            expand_session_variables_with("proj-${AGENT_NAME}-$GIT_BRANCH", &extra),
            format!("proj-my-agent-{}", git_branch())
        );
    }

    #[test]
    fn test_extra_vars_override_env() {
        std::env::set_var("HARNX_TEST_OVERRIDE", "from-env");
        let extra = std::collections::HashMap::from([("HARNX_TEST_OVERRIDE", "from-extra")]);
        assert_eq!(
            expand_session_variables_with("$HARNX_TEST_OVERRIDE", &extra),
            "from-extra"
        );
        std::env::remove_var("HARNX_TEST_OVERRIDE");
    }

    #[test]
    fn test_extra_vars_fallback_to_env() {
        std::env::set_var("HARNX_TEST_ENVONLY", "env-val");
        let extra = std::collections::HashMap::from([("AGENT_NAME", "agent")]);
        assert_eq!(
            expand_session_variables_with("$AGENT_NAME-$HARNX_TEST_ENVONLY", &extra),
            "agent-env-val"
        );
        std::env::remove_var("HARNX_TEST_ENVONLY");
    }
}
