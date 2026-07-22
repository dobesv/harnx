use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use wait_timeout::ChildExt;

use crate::auth::{AppAuthConfig, AuthConfig, AuthSource, RepoConfig, DEFAULT_GITHUB_API_URL};
use crate::ratelimit::RateLimitConfig;
use crate::store_github::GitHubStoreConfig;

const DEFAULT_RETENTION_DAYS: u64 = 14;
const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 3000;
const DEFAULT_PLAN_LABEL: &str = "harnx-plan";
pub const GITHUB_HOST: &str = "github.com";
const GIT_DETECTION_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub auth: AuthConfig,
    pub default_repo: Option<RepoConfig>,
    pub store: GitHubStoreConfig,
    pub rate_limit: RateLimitConfig,
    pub retention_days: u64,
    pub http: bool,
    pub host: String,
    pub port: u16,
}

impl AppConfig {
    pub fn parse_from_env_and_args() -> Result<Self> {
        Self::parse_from_with(
            std::env::args().skip(1),
            |key| std::env::var(key).ok(),
            detect_github_repo_from_git_origin,
        )
    }

    pub fn parse_from<I, S, F>(args: I, env: F) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
        F: Fn(&str) -> Option<String>,
    {
        Self::parse_from_with(args, env, detect_github_repo_from_git_origin)
    }

    pub fn parse_from_with<I, S, F, G>(args: I, env: F, origin_provider: G) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
        F: Fn(&str) -> Option<String>,
        G: Fn() -> Result<String>,
    {
        let mut token_arg: Option<String> = None;
        let mut base_url_arg: Option<String> = None;
        let mut max_wait_secs_arg: Option<u64> = None;
        let mut retention_days_arg: Option<u64> = None;
        let mut plan_label_arg: Option<String> = None;
        let mut delete_behavior_arg: Option<bool> = None;
        let mut http = false;
        let mut host: Option<String> = None;
        let mut port: Option<u16> = None;

        let args: Vec<String> = args.into_iter().map(Into::into).collect();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--token" => {
                    token_arg = Some(next_value(&args, &mut i, "--token")?);
                }
                "--api-url" => {
                    base_url_arg = Some(next_value(&args, &mut i, "--api-url")?);
                }
                "--max-wait-secs" => {
                    max_wait_secs_arg = Some(parse_u64_flag(&args, &mut i, "--max-wait-secs")?);
                }
                "--retention-days" | "-r" => {
                    retention_days_arg = Some(parse_u64_flag(&args, &mut i, "--retention-days")?);
                }
                "--plan-label" => {
                    plan_label_arg = Some(next_value(&args, &mut i, "--plan-label")?);
                }
                "--delete-behavior" => {
                    delete_behavior_arg = Some(parse_delete_behavior(&next_value(
                        &args,
                        &mut i,
                        "--delete-behavior",
                    )?)?);
                }
                "--http" => {
                    http = true;
                    i += 1;
                }
                "--host" => {
                    host = Some(next_value(&args, &mut i, "--host")?);
                }
                "--port" => {
                    port = Some(parse_u16_flag(&args, &mut i, "--port")?);
                }
                "--help" | "-h" => print_help_and_exit(),
                other => {
                    bail!("harnx-mcp-plans-github: unknown argument: {other}");
                }
            }
        }

        let default_repo = match origin_provider().and_then(|origin| parse_github_origin(&origin)) {
            Ok(repo) => Some(repo),
            Err(err) => {
                eprintln!(
                    "harnx-mcp-plans-github: warning: could not detect default GitHub repository from git origin: {err}"
                );
                None
            }
        };

        let base_url = first_non_empty(base_url_arg, env("GITHUB_API_URL"))
            .unwrap_or_else(|| DEFAULT_GITHUB_API_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        let auth_source = match first_non_empty(token_arg, env("GITHUB_TOKEN")) {
            Some(token) => AuthSource::PersonalAccessToken(token),
            None => match app_auth_from_env(&env)? {
                Some(app) => AuthSource::GitHubApp(app),
                None => bail!("set GITHUB_TOKEN or GitHub App env vars"),
            },
        };

        let retention_days = if let Some(days) = retention_days_arg {
            days
        } else if let Some(value) = env("AGENT_PLANS_RETENTION_DAYS") {
            parse_u64_env("AGENT_PLANS_RETENTION_DAYS", &value)?
        } else {
            DEFAULT_RETENTION_DAYS
        };

        let plan_label = first_non_empty(plan_label_arg, env("GITHUB_PLAN_LABEL"))
            .unwrap_or_else(|| DEFAULT_PLAN_LABEL.to_string());
        if plan_label.trim().is_empty() {
            bail!("plan label must not be empty");
        }

        let delete_is_close = if let Some(flag) = delete_behavior_arg {
            flag
        } else if let Some(value) = env("GITHUB_DELETE_BEHAVIOR") {
            parse_delete_behavior(&value)?
        } else {
            true
        };

        let max_wait_secs = if let Some(value) = max_wait_secs_arg {
            value
        } else if let Some(value) = env("GITHUB_MAX_WAIT_SECS") {
            parse_u64_env("GITHUB_MAX_WAIT_SECS", &value)?
        } else {
            RateLimitConfig::default().max_wait_secs
        };

        let host = host.unwrap_or_else(|| DEFAULT_HOST.to_string());
        let port = port.unwrap_or(DEFAULT_PORT);

        Ok(Self {
            auth: AuthConfig {
                base_url,
                repo: default_repo.clone().unwrap_or_else(|| RepoConfig {
                    owner: String::new(),
                    repo: String::new(),
                }),
                source: auth_source,
            },
            default_repo,
            store: GitHubStoreConfig {
                plan_label,
                delete_is_close,
            },
            rate_limit: RateLimitConfig {
                max_wait_secs,
                ..RateLimitConfig::default()
            },
            retention_days,
            http,
            host,
            port,
        })
    }
}

pub fn parse_github_origin(url: &str) -> Result<RepoConfig> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        bail!("git origin URL is empty")
    }

    if let Some(rest) = trimmed.strip_prefix("git@") {
        let (host, path) = rest
            .split_once(':')
            .ok_or_else(|| anyhow!("unparseable git origin URL: {trimmed}"))?;
        ensure_github_host(host)?;
        return parse_owner_repo_path(path, trimmed);
    }

    if let Ok(parsed) = reqwest::Url::parse(trimmed) {
        let host = parsed
            .host_str()
            .ok_or_else(|| anyhow!("unparseable git origin URL: {trimmed}"))?;
        ensure_github_host(host)?;
        let path = parsed.path().trim_matches('/');
        return parse_owner_repo_path(path, trimmed);
    }

    bail!("unparseable git origin URL: {trimmed}")
}

fn ensure_github_host(host: &str) -> Result<()> {
    if host.eq_ignore_ascii_case(GITHUB_HOST) {
        Ok(())
    } else {
        bail!("git origin host must be {GITHUB_HOST}, got: {host}")
    }
}

fn parse_owner_repo_path(path: &str, original_url: &str) -> Result<RepoConfig> {
    let trimmed = path.trim_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let mut parts = trimmed.split('/');
    let owner = parts.next().unwrap_or_default().trim();
    let repo = parts.next().unwrap_or_default().trim();

    if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
        bail!("unparseable GitHub origin repository path in URL: {original_url}")
    }

    Ok(RepoConfig {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
    })
}

fn detect_github_repo_from_git_origin() -> Result<String> {
    detect_github_repo_from_git_origin_in(std::env::current_dir()?.as_path())
}

fn detect_github_repo_from_git_origin_in(dir: &Path) -> Result<String> {
    let inside = run_git_with_timeout(dir, ["rev-parse", "--is-inside-work-tree"])
        .context("run git rev-parse --is-inside-work-tree")?;

    if !inside.status.success() {
        bail!("current working directory is not inside a git repository")
    }

    let origin = run_git_with_timeout(dir, ["remote", "get-url", "origin"])
        .context("run git remote get-url origin")?;

    if !origin.status.success() {
        let stderr = String::from_utf8_lossy(&origin.stderr);
        if stderr.contains("No such remote") {
            bail!("git repository has no 'origin' remote configured")
        }
        bail!(
            "failed to read git origin remote URL: {}",
            stderr.trim().trim_end_matches('.')
        )
    }

    let stdout = String::from_utf8(origin.stdout).context("git origin URL was not valid UTF-8")?;
    let origin_url = stdout.trim();
    if origin_url.is_empty() {
        bail!("git origin URL is empty")
    }
    Ok(origin_url.to_owned())
}

fn run_git_with_timeout<I, S>(dir: &Path, args: I) -> Result<std::process::Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut child = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn git")?;

    match child
        .wait_timeout(GIT_DETECTION_TIMEOUT)
        .context("wait for git process")?
    {
        Some(_) => child.wait_with_output().context("collect git output"),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("git origin detection timed out")
        }
    }
}

fn app_auth_from_env<F>(env: &F) -> Result<Option<AppAuthConfig>>
where
    F: Fn(&str) -> Option<String>,
{
    let app_id = first_non_empty(None, env("GITHUB_APP_ID"));
    let private_key = first_non_empty(None, env("GITHUB_APP_PRIVATE_KEY"));
    let installation_id = first_non_empty(None, env("GITHUB_APP_INSTALLATION_ID"));

    match (app_id, private_key, installation_id) {
        (None, None, None) => Ok(None),
        (Some(app_id), Some(private_key), Some(installation_id)) => Ok(Some(AppAuthConfig {
            app_id,
            private_key_pem: crate::auth::load_private_key(&private_key)
                .context("load GitHub App private key")?,
            installation_id,
        })),
        _ => bail!(
            "GitHub App auth requires GITHUB_APP_ID, GITHUB_APP_PRIVATE_KEY, GITHUB_APP_INSTALLATION_ID"
        ),
    }
}

fn first_non_empty(a: Option<String>, b: Option<String>) -> Option<String> {
    a.into_iter()
        .chain(b)
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
}

fn next_value(args: &[String], index: &mut usize, flag: &str) -> Result<String> {
    if *index + 1 >= args.len() {
        bail!("harnx-mcp-plans-github: {flag} requires a value");
    }
    let value = args[*index + 1].clone();
    *index += 2;
    Ok(value)
}

fn parse_u64_flag(args: &[String], index: &mut usize, flag: &str) -> Result<u64> {
    let value = next_value(args, index, flag)?;
    parse_u64_value(flag, &value)
}

fn parse_u16_flag(args: &[String], index: &mut usize, flag: &str) -> Result<u16> {
    let value = next_value(args, index, flag)?;
    value.parse::<u16>().with_context(|| {
        format!("harnx-mcp-plans-github: {flag} requires a valid port number (got: {value})")
    })
}

fn parse_u64_env(name: &str, value: &str) -> Result<u64> {
    parse_u64_value(name, value)
}

fn parse_u64_value(name: &str, value: &str) -> Result<u64> {
    value.trim().parse::<u64>().with_context(|| {
        format!("harnx-mcp-plans-github: {name} requires a non-negative integer (got: {value})")
    })
}

fn parse_delete_behavior(value: &str) -> Result<bool> {
    match value.trim() {
        "close" => Ok(true),
        "leave" => Ok(false),
        other => bail!(
            "harnx-mcp-plans-github: --delete-behavior/GITHUB_DELETE_BEHAVIOR must be 'close' or 'leave' (got: {other})"
        ),
    }
}

fn print_help_and_exit() -> ! {
    eprintln!("harnx-mcp-plans-github: GitHub Issues-backed plan/task/note MCP server");
    eprintln!();
    eprintln!("Usage: harnx-mcp-plans-github [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --token <token>             GitHub PAT (env: GITHUB_TOKEN)");
    eprintln!("  --api-url <url>             GitHub API base URL (env: GITHUB_API_URL)");
    eprintln!("  --max-wait-secs <N>         Max rate-limit wait before failing (env: GITHUB_MAX_WAIT_SECS)");
    eprintln!("  --retention-days, -r <N>    Close stale plan issues after N days; 0 disables (env: AGENT_PLANS_RETENTION_DAYS)");
    eprintln!("  --plan-label <label>        Label used to mark plan issues (env: GITHUB_PLAN_LABEL, default: harnx-plan)");
    eprintln!("  --delete-behavior <mode>    close|leave for delete operations (env: GITHUB_DELETE_BEHAVIOR, default: close)");
    eprintln!("  --http                      Serve MCP over Streamable HTTP at /mcp");
    eprintln!("  --host <addr>               Bind address for HTTP mode (default: 127.0.0.1; set explicitly for wider exposure)");
    eprintln!("  --port <N>                  Bind port for HTTP mode (default: 3000)");
    eprintln!("  --help, -h                  Show this help message");
    eprintln!();
    eprintln!(
        "Repository target is auto-detected from git origin in current working directory ({GITHUB_HOST} only)."
    );
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthSource;
    use std::collections::HashMap;
    use tempfile::tempdir;

    #[test]
    fn parse_pat_config_from_flags_and_env() {
        let mut env = HashMap::new();
        env.insert("AGENT_PLANS_RETENTION_DAYS".to_string(), "21".to_string());
        let cfg = AppConfig::parse_from_with(
            ["--token", "secret", "--plan-label", "custom-plan"],
            |key| env.get(key).cloned(),
            || Ok(format!("git@{GITHUB_HOST}:acme/plans.git")),
        )
        .unwrap();

        assert_eq!(cfg.auth.repo.owner, "acme");
        assert_eq!(cfg.auth.repo.repo, "plans");
        assert!(matches!(
            cfg.auth.source,
            AuthSource::PersonalAccessToken(_)
        ));
        assert_eq!(cfg.store.plan_label, "custom-plan");
        assert_eq!(cfg.retention_days, 21);
        assert!(cfg.store.delete_is_close);
    }

    #[test]
    fn parse_delete_behavior_leave() {
        let mut env = HashMap::new();
        env.insert("GITHUB_TOKEN".to_string(), "secret".to_string());
        let cfg = AppConfig::parse_from_with(
            ["--delete-behavior", "leave"],
            |key| env.get(key).cloned(),
            || Ok(format!("https://{GITHUB_HOST}/acme/plans")),
        )
        .unwrap();
        assert!(!cfg.store.delete_is_close);
    }

    #[test]
    fn parse_base_url_trims_trailing_slash() {
        let mut env = HashMap::new();
        env.insert("GITHUB_TOKEN".to_string(), "secret".to_string());
        let cfg = AppConfig::parse_from_with(
            ["--api-url", "https://example.test/api///"],
            |key| env.get(key).cloned(),
            || Ok(format!("https://{GITHUB_HOST}/acme/plans/")),
        )
        .unwrap();

        assert_eq!(cfg.auth.base_url, "https://example.test/api");
    }

    #[test]
    fn parse_http_defaults_to_loopback_host() {
        let mut env = HashMap::new();
        env.insert("GITHUB_TOKEN".to_string(), "secret".to_string());
        let cfg = AppConfig::parse_from_with(
            ["--http"],
            |key| env.get(key).cloned(),
            || Ok(format!("git@{GITHUB_HOST}:acme/plans")),
        )
        .unwrap();

        assert_eq!(cfg.host, "127.0.0.1");
    }

    #[test]
    fn parse_github_origin_accepts_ssh() {
        let repo = parse_github_origin(&format!("git@{GITHUB_HOST}:owner/repo.git")).unwrap();
        assert_eq!(repo.owner, "owner");
        assert_eq!(repo.repo, "repo");
    }

    #[test]
    fn parse_github_origin_accepts_https() {
        let repo = parse_github_origin(&format!("https://{GITHUB_HOST}/owner/repo.git")).unwrap();
        assert_eq!(repo.owner, "owner");
        assert_eq!(repo.repo, "repo");
    }

    #[test]
    fn parse_github_origin_accepts_https_without_git_suffix() {
        let repo = parse_github_origin(&format!("http://{GITHUB_HOST}/owner/repo")).unwrap();
        assert_eq!(repo.owner, "owner");
        assert_eq!(repo.repo, "repo");
    }

    #[test]
    fn parse_github_origin_accepts_trailing_slash() {
        let repo = parse_github_origin(&format!("https://{GITHUB_HOST}/owner/repo/")).unwrap();
        assert_eq!(repo.owner, "owner");
        assert_eq!(repo.repo, "repo");
    }

    #[test]
    fn parse_github_origin_rejects_non_github_host() {
        let err = parse_github_origin("https://gitlab.com/owner/repo.git").unwrap_err();
        assert!(err.to_string().contains("gitlab.com"));
    }

    #[test]
    fn parse_github_origin_rejects_garbage() {
        let err = parse_github_origin("not-a-url").unwrap_err();
        assert!(err.to_string().contains("unparseable"));
    }

    #[test]
    fn parse_github_origin_rejects_empty_owner_or_repo() {
        assert!(parse_github_origin(&format!("https://{GITHUB_HOST}//repo")).is_err());
        assert!(parse_github_origin(&format!("https://{GITHUB_HOST}/owner/")).is_err());
    }

    #[test]
    fn detect_github_repo_from_git_origin_in_real_repo() {
        let repo_dir = tempdir().unwrap();
        git(repo_dir.path(), ["init"]).unwrap();
        git(
            repo_dir.path(),
            [
                "remote",
                "add",
                "origin",
                &format!("git@{GITHUB_HOST}:acme/plans.git"),
            ],
        )
        .unwrap();

        let origin = detect_github_repo_from_git_origin_in(repo_dir.path()).unwrap();
        let repo = parse_github_origin(&origin).unwrap();
        assert_eq!(repo.owner, "acme");
        assert_eq!(repo.repo, "plans");
    }

    #[test]
    fn detect_github_repo_from_git_origin_in_non_git_dir_errors_clearly() {
        let repo_dir = tempdir().unwrap();
        let err = detect_github_repo_from_git_origin_in(repo_dir.path()).unwrap_err();
        assert!(err
            .to_string()
            .contains("current working directory is not inside a git repository"));
    }

    #[test]
    fn detect_github_repo_from_git_origin_in_repo_without_origin_errors_clearly() {
        let repo_dir = tempdir().unwrap();
        git(repo_dir.path(), ["init"]).unwrap();

        let err = detect_github_repo_from_git_origin_in(repo_dir.path()).unwrap_err();
        assert!(err
            .to_string()
            .contains("git repository has no 'origin' remote configured"));
    }

    fn git<I, S>(dir: &Path, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .context("run git in test repo")?;
        if status.success() {
            Ok(())
        } else {
            bail!("git command failed in test repo")
        }
    }
}
