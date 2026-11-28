//! Shared plumbing for the tool-template integration tests: the
//! discover-then-build toolset pair every template invocation test starts with.
use harnx_bash_tools::{discover_tool_templates, BashToolset};
use harnx_sandbox_common::SandboxConfig;
use harnx_tool_allow::ResolvedAllowlist;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) fn sandbox_config_for(root: &Path) -> SandboxConfig {
    let mut allowlist = ResolvedAllowlist::new();
    allowlist.insert_rwx(root);
    SandboxConfig {
        enabled: false,
        allowlist: Arc::new(allowlist),
        extra_env_passthrough: vec![],
        env_overrides: vec![],
        sandbox_run_path: PathBuf::from("harnx-sandbox-exec"),
    }
}

/// Discovers templates from the given paths and builds the toolset over the
/// granted root.
pub(crate) async fn toolset_for(root: &Path, template_paths: &[PathBuf]) -> BashToolset {
    let templates =
        discover_tool_templates(None, template_paths, &[]).expect("discover command template");
    BashToolset::new(sandbox_config_for(root), templates)
        .await
        .expect("build toolset")
}
