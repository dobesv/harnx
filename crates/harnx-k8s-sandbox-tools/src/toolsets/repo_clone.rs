use super::Gateway;
use harnx_toolset::ToolInvokeError;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Deserialize)]
pub(super) struct RepoSpec {
    pub(super) repo_url: String,
    #[serde(default)]
    pub(super) branch: Option<String>,
    #[serde(default)]
    pub(super) path: Option<String>,
}

#[derive(Serialize)]
pub(super) struct CloneResult {
    clone_path: String,
    branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub(super) struct CloneRequest<'a> {
    pub(super) sandbox_id: &'a str,
    pub(super) endpoint: &'a str,
    pub(super) repo: RepoSpec,
    pub(super) cancel: CancellationToken,
}

struct RemoteClone<'a> {
    sandbox_id: &'a str,
    endpoint: &'a str,
    command: &'a str,
    cancel: CancellationToken,
}

enum CloneAttempt {
    Success(String),
    Failed(String),
}

pub(super) async fn clone_repo(
    gateway: &Gateway,
    request: CloneRequest<'_>,
) -> Result<CloneResult, ToolInvokeError> {
    let clone_path = match clone_path(&request.repo) {
        Ok(path) => path,
        Err(error) => {
            return Ok(CloneResult {
                clone_path: request.repo.path.unwrap_or_default(),
                branch: String::new(),
                error: Some(error),
            });
        }
    };
    let command = clone_command(&request.repo, &clone_path);
    let mut last_error = String::new();
    for attempt in 1..=3 {
        let remote = RemoteClone {
            sandbox_id: request.sandbox_id,
            endpoint: request.endpoint,
            command: &command,
            cancel: request.cancel.clone(),
        };
        match clone_once(gateway, remote).await? {
            CloneAttempt::Success(branch) => {
                return Ok(CloneResult {
                    clone_path,
                    branch,
                    error: None,
                });
            }
            CloneAttempt::Failed(error) => last_error = error,
        }
        if attempt == 3 || !retryable_clone_error(&last_error) {
            break;
        }
        wait_before_retry(attempt, &request.cancel).await?;
    }
    Ok(CloneResult {
        clone_path,
        branch: String::new(),
        error: Some(annotated_error(last_error)),
    })
}

async fn clone_once(
    gateway: &Gateway,
    request: RemoteClone<'_>,
) -> Result<CloneAttempt, ToolInvokeError> {
    let result = gateway
        .call_remote(
            request.sandbox_id,
            request.endpoint,
            "bash_exec",
            Map::from_iter([
                (
                    "command".to_string(),
                    Value::String(request.command.to_string()),
                ),
                (
                    "working_dir".to_string(),
                    Value::String("/workspace".to_string()),
                ),
            ]),
            BTreeSet::new(),
            request.cancel,
        )
        .await;
    match result {
        Ok(result) => Ok(parse_clone_result(result)),
        Err(error @ ToolInvokeError::Fatal(_)) => Err(error),
        Err(error @ ToolInvokeError::Recoverable(_)) => Ok(CloneAttempt::Failed(error.to_string())),
    }
}

fn parse_clone_result(result: Value) -> CloneAttempt {
    let text = result_text(&result);
    if result_is_success(&result, &text) {
        let branch = stdout_text(&text)
            .lines()
            .last()
            .unwrap_or("HEAD")
            .trim()
            .to_string();
        CloneAttempt::Success(branch)
    } else {
        CloneAttempt::Failed(text)
    }
}

async fn wait_before_retry(
    attempt: u64,
    cancel: &CancellationToken,
) -> Result<(), ToolInvokeError> {
    tokio::select! {
        _ = cancel.cancelled() => {
            Err(ToolInvokeError::Fatal("tool call cancelled".to_string()))
        }
        () = tokio::time::sleep(Duration::from_secs(attempt * 2)) => Ok(()),
    }
}

fn annotated_error(mut error: String) -> String {
    if auth_clone_error(&error) {
        error.push_str("\n\nThe repository URL is likely correct. On a private repository, 'Repository not found' usually indicates a transient credential or GitHub App access issue. The clone was retried automatically.");
    }
    error
}

pub(super) fn clone_path(repo: &RepoSpec) -> Result<String, String> {
    let path = repo.path.clone().unwrap_or_else(|| {
        let trimmed = repo.repo_url.trim_end_matches('/').trim_end_matches(".git");
        let name = trimmed
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or("repo");
        format!("/workspace/{name}")
    });
    if !path.starts_with("/workspace/")
        || path
            .split('/')
            .any(|component| matches!(component, "." | ".."))
    {
        return Err(
            "clone path must be an absolute path below /workspace without '.' or '..' components"
                .to_string(),
        );
    }
    Ok(path)
}

pub(super) fn clone_command(repo: &RepoSpec, path: &str) -> String {
    let url = shell_words::quote(&repo.repo_url);
    let path = shell_words::quote(path);
    let branch = repo
        .branch
        .as_deref()
        .filter(|branch| !branch.is_empty())
        .map(|branch| format!(" --branch {}", shell_words::quote(branch)))
        .unwrap_or_default();
    format!(
        "if test ! -d {path}/.git; then git clone{branch} -- {url} {path}; fi && git -C {path} rev-parse --abbrev-ref HEAD"
    )
}

pub(super) fn result_text(result: &Value) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|content| content.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn stdout_text(text: &str) -> &str {
    text.split_once("<!-- start stdout -->\n```\n")
        .and_then(|(_, tail)| tail.split_once("\n```\n<!-- end stdout -->"))
        .map_or("", |(stdout, _)| stdout)
}

pub(super) fn result_is_success(result: &Value, text: &str) -> bool {
    result.get("isError") != Some(&Value::Bool(true)) && text.contains("exit_code: 0")
}

pub(super) fn retryable_clone_error(error: &str) -> bool {
    const SIGNATURES: &[&str] = &[
        "repository not found",
        "authentication failed",
        "could not read username",
        "unable to access",
        "bad credentials",
        "could not resolve host",
        "connection refused",
        "connection reset",
        "timed out",
        "tls",
        "from proxy",
        "could not resolve proxy",
    ];
    let error = error.to_lowercase();
    SIGNATURES.iter().any(|signature| error.contains(signature))
}

pub(super) fn auth_clone_error(error: &str) -> bool {
    let error = error.to_lowercase();
    [
        "repository not found",
        "authentication failed",
        "could not read username",
        "unable to access",
        "bad credentials",
    ]
    .iter()
    .any(|signature| error.contains(signature))
}

#[cfg(test)]
#[path = "repo_clone_tests.rs"]
mod tests;
