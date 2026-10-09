//! Agent authorization through the real HTTP entry point, including dispatch bypasses.

use anyhow::{bail, Context, Result};
use reqwest::{Client, Method, Response, StatusCode};
use serde_json::Value;
use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;

const RULES: &str = "rules:\n  - agents: [allowed]\n    scopes: [prompt]\n    users: [alice]\n  - agents: [blocked]\n    scopes: [admin]\n    users: [admin]\n";
const FIXTURE_ENV_KEYS: [&str; 6] = ["PATH", "HOME", "SYSTEMROOT", "WINDIR", "TEMP", "TMP"];

fn is_fixture_env_key(key: &std::ffi::OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };

    if cfg!(windows) {
        FIXTURE_ENV_KEYS
            .iter()
            .any(|allowed| key.eq_ignore_ascii_case(allowed))
    } else {
        FIXTURE_ENV_KEYS.contains(&key)
    }
}

#[test]
fn fixture_environment_keeps_platform_keys_but_drops_ambient_credentials() {
    assert!(is_fixture_env_key(std::ffi::OsStr::new("PATH")));
    assert!(is_fixture_env_key(std::ffi::OsStr::new("HOME")));
    assert!(!is_fixture_env_key(std::ffi::OsStr::new("HTTP_PROXY")));
    assert!(!is_fixture_env_key(std::ffi::OsStr::new("OPENAI_API_KEY")));

    #[cfg(windows)]
    for key in ["Path", "sYsTeMrOoT", "windir", "Temp", "tMp"] {
        assert!(is_fixture_env_key(std::ffi::OsStr::new(key)), "{key}");
    }
}

fn write_fixture_config(root: &std::path::Path, nats_url: Option<&str>) -> Result<()> {
    for dir in [
        "config/agents",
        "config/packages/coding/agents",
        "config/clients",
        "config/nats_servers",
        "data",
        "state",
        "assets",
    ] {
        fs::create_dir_all(root.join(dir))?;
    }
    fs::write(root.join("config/config.yaml"), "model: openai:test\n")?;
    fs::write(
        root.join("config/clients/openai.yaml"),
        "type: openai\napi_key: test\nmodels:\n  - name: test\n    type: chat\n    max_input_tokens: 4096\n",
    )?;
    for agent in ["allowed", "blocked", "sisyphus"] {
        fs::write(
            root.join(format!("config/agents/{agent}.md")),
            "---\nmodel: openai:test\nrole: assistant\n---\nTest agent.\n",
        )?;
    }
    fs::write(
        root.join("config/packages/coding/agents/coder.md"),
        "---\nmodel: /openai:test\nrole: assistant\n---\nTest package agent.\n",
    )?;
    for cluster in ["default", "remote"] {
        let url = if cluster == "default" {
            nats_url.unwrap_or("nats://127.0.0.1:1")
        } else {
            "nats://127.0.0.1:1"
        };
        fs::write(
            root.join(format!("config/nats_servers/{cluster}.yaml")),
            format!("url: {url}\nagents:\n  - name: sisyphus\n    role: assistant\n    description: Remote agent\n  - name: allowed\n    role: assistant\n  - name: blocked\n    role: assistant\n"),
        )?;
    }
    fs::write(root.join("assets/index.html"), "public SPA shell")?;
    Ok(())
}

#[derive(Default)]
pub(super) struct MembershipSettings<'a> {
    pub config: &'a str,
    pub args: &'a [&'a str],
    pub env: &'a [(&'a str, &'a str)],
}

pub(super) struct Fixture {
    child: Child,
    root: TempDir,
    pub(super) base: String,
    pub(super) client: Client,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Fixture {
    async fn start(rules: Option<&str>, default_cluster: Option<&str>) -> Result<Self> {
        Self::start_with_settings(rules, default_cluster, None, MembershipSettings::default()).await
    }

    pub(super) async fn start_with_nats(rules: Option<&str>, nats_url: &str) -> Result<Self> {
        Self::start_with_settings(
            rules,
            Some("default"),
            Some(nats_url),
            MembershipSettings::default(),
        )
        .await
    }

    pub(super) async fn start_memberships(
        rules: Option<&str>,
        nats_url: Option<&str>,
        settings: MembershipSettings<'_>,
    ) -> Result<Self> {
        Self::start_with_settings(rules, Some("default"), nats_url, settings).await
    }

    async fn start_with_settings(
        rules: Option<&str>,
        default_cluster: Option<&str>,
        nats_url: Option<&str>,
        settings: MembershipSettings<'_>,
    ) -> Result<Self> {
        harnx_core::require_nextest();
        let root = tempfile::tempdir()?;
        write_fixture_config(root.path(), nats_url)?;
        let config_path = root.path().join("config/config.yaml");
        let config = fs::read_to_string(&config_path)?;
        fs::write(&config_path, format!("{config}\n{}", settings.config))?;
        if let Some(rules) = rules {
            fs::write(root.path().join("config/access.yaml"), rules)?;
        }
        let log_path = root.path().join("server.log");
        let log = fs::File::create(&log_path)?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_harnx-serve"));
        command
            .env_clear()
            .envs(std::env::vars_os().filter(|(key, _)| is_fixture_env_key(key)))
            .current_dir(root.path())
            .env("HARNX_CONFIG_DIR", root.path().join("config"))
            .env("HARNX_DATA_DIR", root.path().join("data"))
            .env("HARNX_STATE_DIR", root.path().join("state"))
            .args([
                "--addr",
                "127.0.0.1:0",
                "--user-id-source",
                "x-user",
                "--web-assets",
            ])
            .arg(root.path().join("assets"))
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        if let Some(cluster) = default_cluster {
            command.env("HARNX_NATS_SERVER", cluster);
        }
        command
            .args(settings.args)
            .envs(settings.env.iter().copied());
        // Loopback requests must reach the fixture without ambient proxy header rewriting.
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(60))
            .build()?;
        let mut fixture = Self {
            child: command.spawn().context("start harnx-serve")?,
            root,
            base: String::new(),
            client,
        };
        fixture.base = fixture.wait_for_base(log_path).await?;
        Ok(fixture)
    }

    async fn wait_for_base(&mut self, log_path: PathBuf) -> Result<String> {
        // Bind :0 and read the actual address instead of racing a free-port probe.
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let log = fs::read_to_string(&log_path)?;
            if let Some(url) = log.lines().find_map(|line| line.strip_prefix("Web UI:")) {
                return Ok(url.trim().trim_end_matches('/').to_string());
            }
            if let Some(status) = self.child.try_wait()? {
                bail!("harnx-serve exited during startup ({status}): {log}");
            }
            if Instant::now() >= deadline {
                bail!("harnx-serve did not bind within 60s: {log}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn request(&self, method: Method, path: &str, user: &str) -> Result<Response> {
        Ok(self
            .client
            .request(method, format!("{}{path}", self.base))
            .header("x-user", user)
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .body("{")
            .send()
            .await?)
    }

    async fn names(&self, user: &str, query: &str) -> Result<Vec<String>> {
        let response = self
            .request(Method::GET, &format!("/v1/agents{query}"), user)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await?;
        Ok(body["data"]
            .as_array()
            .expect("agent list")
            .iter()
            .map(|agent| agent["name"].as_str().expect("agent name").to_string())
            .collect())
    }

    async fn html(&self, agent: &str, user: &str) -> Result<Response> {
        Ok(self
            .client
            .get(format!("{}/v1/agents/{agent}", self.base))
            .header("x-user", user)
            .header("accept", "text/html")
            .send()
            .await?)
    }
}

#[tokio::test]
async fn access_agents_list_only_visible_agents_for_prompt_and_admin() -> Result<()> {
    let fixture = Fixture::start(Some(RULES), None).await?;
    for query in ["", "?role=assistant"] {
        assert_eq!(fixture.names("alice", query).await?, ["allowed"]);
        assert_eq!(fixture.names("admin", query).await?, ["blocked"]);
        assert!(fixture.names("unmatched", query).await?.is_empty());
    }
    assert_eq!(
        fixture.html("allowed", "alice").await?.status(),
        StatusCode::OK
    );
    assert_eq!(
        fixture.html("blocked", "admin").await?.status(),
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn access_hidden_agent_routes_are_identical_to_unknown_agent() -> Result<()> {
    let fixture = Fixture::start(Some(RULES), None).await?;
    let routes = [
        (Method::GET, ""),
        (Method::GET, "/sessions"),
        (Method::POST, "/sessions"),
        (Method::GET, "/sessions/test"),
        (Method::POST, "/sessions/test"),
        (Method::GET, "/sessions/test/events"),
        (Method::GET, "/sessions/test/metadata"),
        (Method::PATCH, "/sessions/test/metadata"),
        (Method::PUT, "/sessions/test/metadata/extensions/test"),
        (Method::DELETE, "/sessions/test/metadata/extensions/test"),
        (Method::GET, "/sessions/test/attachments/not-a-cid"),
        (Method::POST, "/sessions/test/attachments"),
        (Method::DELETE, "/sessions/test/attachments"),
        (Method::GET, "/unknown-subroute"),
    ];
    let mut hidden_bodies = Vec::new();
    for (method, suffix) in &routes {
        let response = fixture
            .request(
                method.clone(),
                &format!("/v1/agents/blocked{suffix}"),
                "alice",
            )
            .await?;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{method} {suffix}"
        );
        assert_eq!(response.headers()["access-control-allow-origin"], "*");
        assert_eq!(response.headers()["content-type"], "application/json");
        hidden_bodies.push(response.text().await?);
    }
    // Same reference and request now point to an unknown agent: bytes must match.
    fs::remove_file(fixture.root.path().join("config/agents/blocked.md"))?;
    for ((method, suffix), hidden) in routes.iter().zip(hidden_bodies) {
        let response = fixture
            .request(
                method.clone(),
                &format!("/v1/agents/blocked{suffix}"),
                "alice",
            )
            .await?;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{method} {suffix}"
        );
        assert_eq!(response.text().await?, hidden, "{method} {suffix}");
    }
    Ok(())
}

#[tokio::test]
async fn access_agent_rules_distinguish_nondefault_cluster_suffix() -> Result<()> {
    let rules = "rules:\n  - agents: ['sisyphus@remote']\n    users: [remote-user]\n  - agents: [sisyphus]\n    users: [bare-user]\n  - agents: ['coding/coder']\n    users: [package-user]\n";
    let fixture = Fixture::start(Some(rules), None).await?;
    assert_eq!(fixture.names("remote-user", "").await?, ["sisyphus@remote"]);
    assert_eq!(fixture.names("bare-user", "").await?, ["sisyphus"]);
    assert_eq!(
        fixture
            .html("sisyphus%40remote", "remote-user")
            .await?
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        fixture
            .html("sisyphus%40remote", "bare-user")
            .await?
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fixture.html("sisyphus", "remote-user").await?.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(fixture.names("package-user", "").await?, ["coding/coder"]);
    assert_eq!(
        fixture
            .html("coding%2Fcoder", "package-user")
            .await?
            .status(),
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn access_agent_rules_match_normalized_default_cluster_ref() -> Result<()> {
    let rules = "rules:\n  - agents: [sisyphus]\n    users: [bare-user]\n  - agents: ['sisyphus@default']\n    users: [canonical-user]\n  - agents: ['sisyphus@remote']\n    users: [remote-user]\n";
    let fixture = Fixture::start(Some(rules), Some("default")).await?;
    assert_eq!(fixture.names("bare-user", "").await?, ["sisyphus"]);
    assert!(fixture.names("canonical-user", "").await?.is_empty());
    assert_eq!(fixture.names("remote-user", "").await?, ["sisyphus@remote"]);
    for agent in ["sisyphus", "sisyphus%40default"] {
        assert_eq!(
            fixture.html(agent, "bare-user").await?.status(),
            StatusCode::OK
        );
        assert_eq!(
            fixture.html(agent, "canonical-user").await?.status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        fixture
            .html("sisyphus%40remote", "bare-user")
            .await?
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fixture
            .html("sisyphus%40remote", "remote-user")
            .await?
            .status(),
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn access_rules_off_preserve_agent_visibility_and_spa_shell() -> Result<()> {
    let fixture = Fixture::start(None, None).await?;
    let names = fixture.names("unmatched", "").await?;
    assert!(names.contains(&"allowed".to_string()));
    assert!(names.contains(&"blocked".to_string()));
    assert_eq!(
        fixture.html("blocked", "unmatched").await?.status(),
        StatusCode::OK
    );
    let missing = fixture
        .request(Method::GET, "/v1/agents/missing", "unmatched")
        .await?;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let protected = Fixture::start(Some(RULES), None).await?;
    let shell = protected
        .client
        .get(format!("{}/agents/blocked/sessions/test", protected.base))
        .header("accept", "text/html")
        .send()
        .await?;
    assert_eq!(shell.status(), StatusCode::OK);
    assert_eq!(shell.text().await?, "public SPA shell");
    Ok(())
}
