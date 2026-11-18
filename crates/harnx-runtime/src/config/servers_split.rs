//! Tool server package loading extracted from config/mod.rs for code health.
use super::*;

impl Config {
    /// Load tool servers from a single package directory.
    ///
    /// Servers are stored with their bare names (the yaml stem) and tagged with
    /// `package = Some(pkg_name)`. The actual display name used with the LLM
    /// — bare or prefixed — is decided at runtime based on which agent is active.
    pub(super) fn load_package_servers(
        config: &mut Config,
        pkg_path: &Path,
        pkg_name: &str,
        patch: Option<&harnx_core::package::PackagePatch>,
    ) -> Result<()> {
        let pkg_tool_dir = pkg_path.join(paths::TOOL_SERVERS_DIR_NAME);
        if !pkg_tool_dir.is_dir() {
            return Ok(());
        }

        for mut server in
            Self::load_tool_servers_with_middleware_base(&pkg_tool_dir, Some(pkg_path))?
        {
            server.package = Some(pkg_name.to_string());
            if let Some(patch) = patch {
                apply_tool_server_patch(&mut server, &patch.tool_servers).with_context(|| {
                    format!("Package patch failed for tool server '{}'", server.name)
                })?;
            }
            config.tool_servers.push(server);
        }
        Ok(())
    }
}

/// Apply a list of jq-style patch expressions to a tool server config.
pub(super) fn apply_tool_server_patch(
    server: &mut ToolServerConfig,
    patches: &[String],
) -> Result<()> {
    if patches.is_empty() {
        return Ok(());
    }

    let saved_package = server.package.clone();
    let input = serde_json::to_value(&*server).context("Failed to serialize tool server config")?;
    let output =
        harnx_core::jaq::eval_filters_strict(patches, input).context("jq patch evaluation")?;
    let mut patched: ToolServerConfig =
        serde_json::from_value(output).context("Failed to deserialize patched tool server")?;
    patched.package = saved_package;
    if let Some(origin) = &server.middleware.origin {
        middleware::bind_middleware(&mut patched.middleware, origin.clone())?;
    }
    *server = patched;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package_server() -> ToolServerConfig {
        ToolServerConfig {
            middleware: Default::default(),
            name: "time".to_string(),
            command: "harnx-time-tools".to_string(),
            args: Vec::new(),
            env: Default::default(),
            enabled: true,
            description: None,
            package: Some("pkg".to_string()),
            hooks: None,
        }
    }

    #[test]
    fn apply_tool_server_patch_changes_field_and_preserves_package() {
        let mut server = package_server();

        apply_tool_server_patch(
            &mut server,
            &[r#".description = "patched" | .args = ["--verbose"]"#.to_string()],
        )
        .expect("apply tool server patch");

        assert_eq!(server.description.as_deref(), Some("patched"));
        assert_eq!(server.args, ["--verbose"]);
        assert_eq!(server.package.as_deref(), Some("pkg"));
    }

    #[test]
    fn apply_tool_server_patch_empty_is_noop() {
        let mut server = package_server();
        let original = server.clone();

        apply_tool_server_patch(&mut server, &[]).expect("empty patch succeeds");

        assert_eq!(server, original);
    }

    #[test]
    fn apply_tool_server_patch_invalid_jq_returns_error() {
        let mut server = package_server();

        let result = apply_tool_server_patch(
            &mut server,
            &[r#".description = "unterminated"#.to_string()],
        );

        assert!(result.is_err());
    }
}
