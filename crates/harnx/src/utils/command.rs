use std::path::Path;
use std::process::Command;

use anyhow::Result;

pub use harnx_core::system_vars::SHELL;

pub fn edit_file(editor: &str, path: &Path) -> Result<()> {
    let mut child = Command::new(editor).arg(path).spawn()?;
    child.wait()?;
    Ok(())
}
