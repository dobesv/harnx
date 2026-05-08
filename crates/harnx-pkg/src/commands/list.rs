use crate::install::{list_installed_packages, load_patch};
use anyhow::Result;
use harnx_core::package::PackageSource;

pub async fn run() -> Result<()> {
    let packages = list_installed_packages()?;
    if packages.is_empty() {
        println!("No packages installed.");
        return Ok(());
    }
    for manifest in &packages {
        let source_str = match &manifest.source {
            PackageSource::Git { url, tag, .. } => format!("git {url} @ {tag}"),
            PackageSource::Oci { url, tag, .. } => format!("oci {url} @ {tag}"),
        };
        let patch_note = match load_patch(&manifest.name) {
            Ok(Some(_)) => " [patched]",
            _ => "",
        };
        println!("{:<24} {source_str}{patch_note}", manifest.name);
    }
    Ok(())
}
