use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use std::collections::HashMap;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

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
pub fn expand_session_variables_with(template: &str, extra_vars: &HashMap<&str, &str>) -> String {
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
/// - `/` and `\` → `-`
/// - Whitespace → `_`
/// - Windows-reserved characters (`<>:"|?*`) → removed
/// - Path-traversal segments (`..`) → removed
/// - Consecutive `-` or `_` → collapsed to a single separator
/// - Strip leading/trailing `-` and `_` characters
/// - Returns `"session"` if the result would be empty
pub fn sanitize_session_name(name: &str) -> String {
    // Replace path separators and whitespace, remove unsafe chars
    let mapped: String = name
        .chars()
        .filter_map(|c| match c {
            '/' | '\\' => Some('-'),
            c if c.is_whitespace() => Some('_'),
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => None,
            other => Some(other),
        })
        .collect();

    // Remove path-traversal segments ("..") by replacing ".." with ""
    let no_traversal = mapped.replace("..", "");

    // Collapse consecutive separators (- or _) into a single one
    let mut result = String::with_capacity(no_traversal.len());
    let mut last_was_sep = false;
    for c in no_traversal.chars() {
        let is_sep = c == '-' || c == '_';
        if is_sep {
            if !last_was_sep {
                result.push(c);
            }
            last_was_sep = true;
        } else {
            result.push(c);
            last_was_sep = false;
        }
    }

    let trimmed = result
        .trim_matches(|c: char| c == '-' || c == '_')
        .to_string();

    if trimmed.is_empty() {
        "session".to_string()
    } else {
        trimmed
    }
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

pub fn git_branch() -> String {
    Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

pub fn git_remote() -> Option<String> {
    Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .and_then(|s| if s.is_empty() { None } else { Some(s) })
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

/// Encode a Unix timestamp (seconds) as a 6-char base64url session ID.
pub fn encode_timestamp_session_id(seconds: u64) -> String {
    URL_SAFE_NO_PAD.encode((seconds as u32).to_be_bytes())
}

/// Decode a 6-char base64url session ID back to Unix seconds. Returns None if not a valid short ID.
pub fn decode_timestamp_session_id(id: &str) -> Option<u64> {
    if id.len() != 6 {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(id).ok()?;
    let bytes: [u8; 4] = bytes.try_into().ok()?;
    Some(u32::from_be_bytes(bytes) as u64)
}

/// Generate a unique session ID starting from current time, retrying +1 second until exists(candidate) is false.
pub fn generate_session_id(exists: impl Fn(&str) -> bool) -> String {
    let mut seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    loop {
        let candidate = encode_timestamp_session_id(seconds);
        if !exists(&candidate) {
            return candidate;
        }
        seconds = seconds.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_session_id_roundtrip() {
        let seconds = 1_735_689_600_u64;
        let id = encode_timestamp_session_id(seconds);
        assert_eq!(id.len(), 6);
        assert_eq!(decode_timestamp_session_id(&id), Some(seconds));
    }

    #[test]
    fn timestamp_session_id_decode_rejects_invalid_inputs() {
        assert_eq!(decode_timestamp_session_id("short"), None);
        assert_eq!(decode_timestamp_session_id("toolong7"), None);
        assert_eq!(decode_timestamp_session_id("!!!!!!"), None);
    }

    #[test]
    fn generate_session_id_retries_on_collision() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let first = encode_timestamp_session_id(now);
        let second = encode_timestamp_session_id(now + 1);
        let generated = generate_session_id(|candidate| candidate == first);

        assert_eq!(generated, second);
        assert!(generated
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

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
        assert_eq!(sanitize_session_name(""), "session");
        assert_eq!(sanitize_session_name("---"), "session");
    }

    #[test]
    fn test_sanitize_backslash() {
        assert_eq!(sanitize_session_name("feat\\branch"), "feat-branch");
    }

    #[test]
    fn test_sanitize_windows_reserved() {
        assert_eq!(sanitize_session_name("my<session>name"), "mysessionname");
        assert_eq!(sanitize_session_name("file:name|test"), "filenametest");
        assert_eq!(sanitize_session_name("a\"b?c*d"), "abcd");
    }

    #[test]
    fn test_sanitize_path_traversal() {
        assert_eq!(sanitize_session_name("../../../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize_session_name("foo/../bar"), "foo-bar");
        assert_eq!(sanitize_session_name(".."), "session");
    }

    #[test]
    fn test_sanitize_collapse_separators() {
        assert_eq!(sanitize_session_name("a//b"), "a-b");
        assert_eq!(sanitize_session_name("a--b__c"), "a-b_c");
    }

    #[test]
    fn test_sanitize_unicode_whitespace() {
        // non-breaking space and other unicode whitespace
        assert_eq!(sanitize_session_name("a\u{00a0}b"), "a_b");
        assert_eq!(sanitize_session_name("a\tb"), "a_b");
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
