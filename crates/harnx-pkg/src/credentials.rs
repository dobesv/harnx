use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use oci_client::secrets::RegistryAuth;
use serde::Deserialize;
use tokio::process::Command;

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum CredentialSource {
    Env { env: String },
    Command { command: String },
    Value { value: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct PackageRepoConfig {
    pub url: String,
    #[serde(default)]
    pub username: Option<CredentialSource>,
    #[serde(default)]
    pub password: Option<CredentialSource>,
}

pub async fn resolve(source: &CredentialSource) -> Result<String> {
    match source {
        CredentialSource::Env { env } => {
            std::env::var(env).with_context(|| format!("env var '{env}' not set"))
        }
        CredentialSource::Command { command } => {
            #[cfg(windows)]
            let output = Command::new("cmd")
                .args(["/C", command])
                .output()
                .await
                .with_context(|| "failed to execute credential command")?;
            #[cfg(not(windows))]
            let output = Command::new("sh")
                .args(["-c", command])
                .output()
                .await
                .with_context(|| "failed to execute credential command")?;

            if !output.status.success() {
                let exit = output
                    .status
                    .code()
                    .map_or_else(|| "signal".to_string(), |code| code.to_string());
                bail!("credential command failed (exit {exit})");
            }

            Ok(String::from_utf8(output.stdout)
                .context("credential command output was not valid UTF-8")?
                .trim()
                .to_string())
        }
        CredentialSource::Value { value } => Ok(value.clone()),
    }
}

fn normalized_prefix(value: &str) -> &str {
    value.trim_end_matches('/')
}

fn stripped_url(url: &str) -> &str {
    url.strip_prefix("oci://")
        .or_else(|| url.strip_prefix("OCI://"))
        .unwrap_or(url)
}

fn split_host_and_path(url: &str) -> (&str, &str) {
    let normalized = normalized_prefix(stripped_url(url));
    match normalized.split_once('/') {
        Some((host, path)) => (host, path),
        None => (normalized, ""),
    }
}

fn is_repo_prefix_match(target: &str, config: &str) -> bool {
    let (target_host, target_path) = split_host_and_path(target);
    let (config_host, config_path) = split_host_and_path(config);

    if !target_host.eq_ignore_ascii_case(config_host) {
        return false;
    }

    if config_path.is_empty() {
        return true;
    }

    if !target_path.starts_with(config_path) {
        return false;
    }

    target_path
        .as_bytes()
        .get(config_path.len())
        .is_none_or(|byte| *byte == b'/')
}

async fn load_repo_config(path: &Path) -> Result<PackageRepoConfig> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("failed to read package repo config {}", path.display()))?;
    serde_yaml::from_str(&raw)
        .with_context(|| format!("failed to parse package repo config {}", path.display()))
}

async fn repo_config_paths(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .with_context(|| format!("failed to read package repo dir {}", dir.display()))?;

    while let Some(entry) = entries
        .next_entry()
        .await
        .with_context(|| "failed to read package repo dir entry")?
    {
        let path = entry.path();
        if entry
            .file_type()
            .await
            .with_context(|| format!("failed to read file type for {}", path.display()))?
            .is_file()
            && path.extension().is_some_and(|ext| ext == "yaml")
        {
            paths.push(path);
        }
    }

    Ok(paths)
}

async fn find_repo_config(url: &str) -> Result<Option<PackageRepoConfig>> {
    let dir = harnx_core::config_paths::package_repos_dir();
    if tokio::fs::metadata(&dir).await.is_err() {
        return Ok(None);
    }

    let target = stripped_url(url);
    let mut matches = Vec::new();
    for path in repo_config_paths(&dir).await? {
        match load_repo_config(&path).await {
            Ok(config) => {
                if is_repo_prefix_match(target, &config.url) {
                    matches.push(config);
                }
            }
            Err(e) => {
                log::warn!(
                    "Skipping malformed credential config {}: {e}",
                    path.display()
                );
            }
        }
    }

    matches.sort_by(|a, b| {
        normalized_prefix(stripped_url(&b.url))
            .len()
            .cmp(&normalized_prefix(stripped_url(&a.url)).len())
    });

    Ok(matches.into_iter().next())
}

pub async fn resolve_oci_auth(url: &str) -> Result<RegistryAuth> {
    let Some(config) = find_repo_config(url).await? else {
        return Ok(RegistryAuth::Anonymous);
    };

    let Some(password_source) = config.password.as_ref() else {
        log::warn!(
            "package repo config matched '{}' but no password was configured; using anonymous auth",
            config.url
        );
        return Ok(RegistryAuth::Anonymous);
    };

    let username = match config.username.as_ref() {
        Some(source) => resolve(source).await?,
        None => String::new(),
    };
    let password = resolve(password_source).await?;

    Ok(RegistryAuth::Basic(username, password))
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::sync::{Mutex, OnceLock};

    use oci_client::secrets::RegistryAuth;

    use super::{find_repo_config, resolve, resolve_oci_auth, CredentialSource};

    static ENV_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

    fn env_mutex() -> &'static Mutex<()> {
        ENV_MUTEX.get_or_init(|| Mutex::new(()))
    }

    fn set_env_var(key: &'static str, value: Option<&str>) -> std::sync::MutexGuard<'static, ()> {
        let guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        match value {
            Some(value) => unsafe { env::set_var(key, value) },
            None => unsafe { env::remove_var(key) },
        }
        guard
    }

    #[test]
    fn resolve_env_var_present() {
        let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        unsafe { env::set_var("HARNX_TEST_CREDENTIAL", "secret-token") };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let resolved = runtime
            .block_on(resolve(&CredentialSource::Env {
                env: "HARNX_TEST_CREDENTIAL".to_string(),
            }))
            .unwrap();
        unsafe { env::remove_var("HARNX_TEST_CREDENTIAL") };

        assert_eq!(resolved, "secret-token");
    }

    #[test]
    fn resolve_env_var_missing() {
        let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        unsafe { env::remove_var("HARNX_TEST_CREDENTIAL_MISSING") };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let err = runtime
            .block_on(resolve(&CredentialSource::Env {
                env: "HARNX_TEST_CREDENTIAL_MISSING".to_string(),
            }))
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("env var 'HARNX_TEST_CREDENTIAL_MISSING' not set"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_command_success() {
        let temp_dir = tempfile::tempdir().unwrap();
        let script_path = temp_dir.path().join("echo-token.sh");
        fs::write(&script_path, "#!/bin/sh\necho mytoken\n").unwrap();

        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();

        let resolved = resolve(&CredentialSource::Command {
            command: script_path.display().to_string(),
        })
        .await
        .unwrap();

        assert_eq!(resolved, "mytoken");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn resolve_command_success() {
        // On Windows use cmd /C echo — the trailing space is trimmed by resolve()
        let resolved = resolve(&CredentialSource::Command {
            command: "echo mytoken".to_string(),
        })
        .await
        .unwrap();

        assert_eq!(resolved, "mytoken");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_command_failure() {
        let err = resolve(&CredentialSource::Command {
            command: "printf 'sensitive output'; exit 12".to_string(),
        })
        .await
        .unwrap_err();

        let message = err.to_string();
        assert!(message.contains("failed (exit 12)"));
        assert!(!message.contains("sensitive output"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn resolve_command_failure() {
        let err = resolve(&CredentialSource::Command {
            command: "exit 12".to_string(),
        })
        .await
        .unwrap_err();

        let message = err.to_string();
        assert!(message.contains("failed (exit"));
    }

    #[tokio::test]
    async fn resolve_value() {
        let resolved = resolve(&CredentialSource::Value {
            value: "literal-secret".to_string(),
        })
        .await
        .unwrap();

        assert_eq!(resolved, "literal-secret");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn prefix_match_most_specific() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("package_repos");
        fs::create_dir_all(&repo_dir).unwrap();
        fs::write(
            repo_dir.join("base.yaml"),
            "url: ghcr.io\npassword:\n  value: base-token\n",
        )
        .unwrap();
        fs::write(
            repo_dir.join("org.yaml"),
            "url: ghcr.io/myorg\npassword:\n  value: org-token\n",
        )
        .unwrap();

        let _guard = set_env_var("HARNX_CONFIG_DIR", temp_dir.path().to_str());
        let found = find_repo_config("ghcr.io/myorg/pkg")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(found.url, "ghcr.io/myorg");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn prefix_match_no_partial_path_segment() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("package_repos");
        fs::create_dir_all(&repo_dir).unwrap();
        fs::write(
            repo_dir.join("ghcr.yaml"),
            "url: ghcr.io/myorg\npassword:\n  value: token\n",
        )
        .unwrap();

        let _guard = set_env_var("HARNX_CONFIG_DIR", temp_dir.path().to_str());
        let found = find_repo_config("ghcr.io/myorg-evil/pkg").await.unwrap();

        assert!(found.is_none());
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn prefix_match_host_case_insensitive() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("package_repos");
        fs::create_dir_all(&repo_dir).unwrap();
        fs::write(
            repo_dir.join("ghcr.yaml"),
            "url: GHCR.IO/myorg\npassword:\n  value: token\n",
        )
        .unwrap();

        let _guard = set_env_var("HARNX_CONFIG_DIR", temp_dir.path().to_str());
        let found = find_repo_config("ghcr.io/myorg/pkg")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(found.url, "GHCR.IO/myorg");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn prefix_match_no_host_suffix() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("package_repos");
        fs::create_dir_all(&repo_dir).unwrap();
        fs::write(
            repo_dir.join("internal.yaml"),
            "url: registry.internal\npassword:\n  value: token\n",
        )
        .unwrap();

        let _guard = set_env_var("HARNX_CONFIG_DIR", temp_dir.path().to_str());
        let found = find_repo_config("registry.internal.attacker.com/pkg")
            .await
            .unwrap();

        assert!(found.is_none());
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn prefix_match_none() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("package_repos");
        fs::create_dir_all(&repo_dir).unwrap();
        fs::write(
            repo_dir.join("ghcr.yaml"),
            "url: ghcr.io/myorg\npassword:\n  value: token\n",
        )
        .unwrap();

        let _guard = set_env_var("HARNX_CONFIG_DIR", temp_dir.path().to_str());
        let found = find_repo_config("example.com/other/pkg").await.unwrap();

        assert!(found.is_none());
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn prefix_match_strips_scheme() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("package_repos");
        fs::create_dir_all(&repo_dir).unwrap();
        fs::write(
            repo_dir.join("ghcr.yaml"),
            "url: ghcr.io/myorg\npassword:\n  value: token\n",
        )
        .unwrap();

        let _guard = set_env_var("HARNX_CONFIG_DIR", temp_dir.path().to_str());
        let found = find_repo_config("oci://ghcr.io/myorg/pkg")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(found.url, "ghcr.io/myorg");
    }

    #[test]
    fn resolve_oci_auth_password_only() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("package_repos");
        fs::create_dir_all(&repo_dir).unwrap();
        fs::write(
            repo_dir.join("ghcr.yaml"),
            "url: ghcr.io/myorg\npassword:\n  value: mytoken\n",
        )
        .unwrap();

        let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let prior = env::var_os("HARNX_CONFIG_DIR");
        unsafe { env::set_var("HARNX_CONFIG_DIR", temp_dir.path()) };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let auth = runtime
            .block_on(resolve_oci_auth("oci://ghcr.io/myorg/pkg"))
            .unwrap();
        match prior {
            Some(value) => unsafe { env::set_var("HARNX_CONFIG_DIR", value) },
            None => unsafe { env::remove_var("HARNX_CONFIG_DIR") },
        }

        assert_eq!(
            auth,
            RegistryAuth::Basic(String::new(), "mytoken".to_string())
        );
    }

    #[test]
    fn resolve_oci_auth_no_creds_warns() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("package_repos");
        fs::create_dir_all(&repo_dir).unwrap();
        fs::write(repo_dir.join("ghcr.yaml"), "url: ghcr.io/myorg\n").unwrap();

        let _guard = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let prior = env::var_os("HARNX_CONFIG_DIR");
        unsafe { env::set_var("HARNX_CONFIG_DIR", temp_dir.path()) };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let auth = runtime
            .block_on(resolve_oci_auth("oci://ghcr.io/myorg/pkg"))
            .unwrap();
        match prior {
            Some(value) => unsafe { env::set_var("HARNX_CONFIG_DIR", value) },
            None => unsafe { env::remove_var("HARNX_CONFIG_DIR") },
        }

        assert_eq!(auth, RegistryAuth::Anonymous);
    }
}
