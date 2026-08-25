pub mod server;
mod tool_template;
mod tool_templates;
mod toolset;

use std::path::{Path, PathBuf};

#[cfg(test)]
mod test_support;

pub use tool_template::ToolTemplate;
pub use toolset::BashToolset;

pub fn discover_tool_templates(
    package_dir: Option<&Path>,
    cli_files: &[PathBuf],
    cli_dirs: &[PathBuf],
) -> anyhow::Result<Vec<ToolTemplate>> {
    tool_template::discover_templates(package_dir, cli_files, cli_dirs)
}
