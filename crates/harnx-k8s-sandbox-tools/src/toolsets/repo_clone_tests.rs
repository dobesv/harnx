use super::*;
use serde_json::json;

fn repo(url: &str, branch: Option<&str>, path: Option<&str>) -> RepoSpec {
    RepoSpec {
        repo_url: url.to_string(),
        branch: branch.map(str::to_string),
        path: path.map(str::to_string),
    }
}

#[test]
fn clone_paths_are_confined_below_the_workspace() {
    let inferred = clone_path(&repo("https://github.com/acme/widgets.git", None, None));
    let explicit = clone_path(&repo(
        "ssh://git@example/repo",
        None,
        Some("/workspace/src"),
    ));
    let empty_name = clone_path(&repo("https://example.invalid/.git", None, None));

    assert_eq!(
        (
            inferred.as_deref(),
            explicit.as_deref(),
            empty_name.as_deref(),
            clone_path(&repo("repo", None, Some("/tmp/repo"))).is_err(),
            clone_path(&repo("repo", None, Some("/workspace/../secret"))).is_err(),
        ),
        (
            Ok("/workspace/widgets"),
            Ok("/workspace/src"),
            Ok("/workspace/repo"),
            true,
            true,
        )
    );
}

#[test]
fn clone_command_quotes_values_and_terminates_options() {
    let url = "https://example.invalid/a repo'; touch /tmp/pwned";
    let command = clone_command(
        &repo(url, Some("feature; false"), None),
        "/workspace/a repo",
    );
    let quoted_url = shell_words::quote(url);
    let option_url = "--upload-pack=evil";
    let option_command = clone_command(&repo(option_url, None, None), "/workspace/repo");
    let quoted_option_url = shell_words::quote(option_url);

    assert_eq!(
        (
            command.contains("'feature; false'"),
            command.contains(&format!(" -- {quoted_url}")),
            shell_words::split(&quoted_url).is_ok_and(|args| args == [url]),
            command.contains("'/workspace/a repo'"),
            option_command.contains(&format!("git clone -- {quoted_option_url} /workspace/repo")),
        ),
        (true, true, true, true, true)
    );
}

#[test]
fn clone_result_parser_uses_the_last_stdout_line_as_the_branch() {
    let text = "execution_id: x\nexit_code: 0\n<!-- start stdout -->\n```\nnoise\nfeature/test\n```\n<!-- end stdout -->";
    let result = json!({
        "content": [{"type": "text", "text": text}]
    });

    assert!(result_is_success(&result, &result_text(&result)));
    assert_eq!(stdout_text(text).lines().last(), Some("feature/test"));
}

#[test]
fn clone_error_classifiers_match_tartarus() {
    for error in [
        "remote: Repository not found.",
        "fatal: Authentication failed",
        "could not read Username",
        "fatal: unable to access repository",
        "Bad credentials",
    ] {
        assert!(auth_clone_error(error), "expected auth error: {error}");
        assert!(retryable_clone_error(error), "expected retry: {error}");
    }
    for error in [
        "could not resolve host: github.com",
        "Connection refused",
        "Connection reset by peer",
        "Operation timed out",
        "TLS handshake failed",
        "response from proxy",
        "could not resolve proxy",
    ] {
        assert!(!auth_clone_error(error), "unexpected auth error: {error}");
        assert!(retryable_clone_error(error), "expected retry: {error}");
    }
    assert!(!retryable_clone_error(
        "fatal: destination path already exists"
    ));
}
