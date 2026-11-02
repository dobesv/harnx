use super::*;

pub(super) struct AttachmentHost {
    pub client: async_nats::Client,
    pub jetstream: async_nats::jetstream::Context,
    pub scope: ServerScope,
    pub root: TempDir,
    child: Child,
    _nats: harnx_test_bins::NatsServerHandle,
}

impl Drop for AttachmentHost {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl AttachmentHost {
    pub async fn start() -> Result<Option<Self>> {
        let Some(nats) = harnx_test_bins::spawn_nats_server().await? else {
            return Ok(None);
        };
        let root = tempfile::tempdir()?;
        let scope = ServerScope::new();
        let client = async_nats::connect(nats.url()).await?;
        let jetstream = async_nats::jetstream::new(client.clone());
        let child = fs_command(root.path(), &scope, nats.url())?
            .spawn()
            .context("spawn NATS filesystem toolset")?;
        let mut host = Self {
            client,
            jetstream,
            scope,
            root,
            child,
            _nats: nats,
        };
        if let Err(error) = assert_registration(&host.client, &host.scope).await {
            if let Some(status) = host.child.try_wait()? {
                anyhow::bail!("filesystem toolset exited before registration: {status}: {error:#}");
            }
            return Err(error);
        }
        Ok(Some(host))
    }

    pub async fn write_attachment(&self, path: &Path, url: &str) -> Result<Value> {
        let reply = invoke(
            &self.client,
            &self.scope,
            "write_attachment",
            json!({
                "path": path.to_string_lossy(), "url": url,
            }),
        )
        .await?;
        let value = reply
            .result
            .map_err(|error| anyhow::anyhow!("write_attachment failed: {error:?}"))?;
        assert_content_envelope(&value);
        assert_eq!(value["isError"], false, "{value}");
        Ok(value)
    }
}

fn fs_command(root: &Path, scope: &ServerScope, nats_url: &str) -> Result<Command> {
    let binary = std::env::var("CARGO_BIN_EXE_harnx-fs-tools")
        .context("CARGO_BIN_EXE_harnx-fs-tools must be set")?;
    let mut command = Command::new(binary);
    // Keep local credentials and allowlist settings out of the child fixture.
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("HARNX_NATS_") || name.starts_with("HARNX_TOOLS_ALLOW_") {
            command.env_remove(key);
        }
    }
    command
        .arg("--allow-write")
        .arg(root)
        .env("HARNX_SERVER_SCOPE", scope.as_str())
        .env("HARNX_NATS_URL", nats_url)
        .env("HARNX_NATS_REPLICAS", "1")
        .env_remove("HARNX_TOOLSET_INCARNATION")
        .env_remove("HARNX_FS_SANDBOX_TRANSPORT")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    Ok(command)
}
