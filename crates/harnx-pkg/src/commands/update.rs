use super::check_updates::{fetcher_for_source, source_subpath, source_tag, source_url};
use crate::cli::UpdateArgs;
use crate::fetch::FetchedPackage;
use crate::install::{install_package, list_installed_packages, load_manifest};
use crate::semver_util::{find_latest_tag, is_newer_version, parse_semver_tag, version_to_tag};
use anyhow::{Context, Result};
use harnx_core::config_paths::{package_dir, packages_dir};
use harnx_core::package::{PackageManifest, PackageSource};

pub async fn run(args: &UpdateArgs) -> Result<()> {
    let packages = if let Some(name) = &args.name {
        vec![load_manifest(name)?]
    } else {
        list_installed_packages()?
    };

    for manifest in packages {
        update_one(manifest).await?;
    }
    Ok(())
}

async fn update_one(manifest: PackageManifest) -> Result<()> {
    let fetcher = fetcher_for_source(&manifest.source);
    let url = source_url(&manifest.source);
    let installed_tag = source_tag(&manifest.source);
    let installed_ver = parse_semver_tag(installed_tag)?;

    let versions = fetcher.list_tags(&url).await?;
    let tag_strings: Vec<String> = versions.iter().map(version_to_tag).collect();
    let latest = find_latest_tag(&tag_strings);

    match latest {
        None => {
            println!("{}: no valid semver tags found upstream", manifest.name);
        }
        Some(v) if !is_newer_version(&installed_ver, &v) => {
            println!("{}: already up to date ({})", manifest.name, installed_tag);
        }
        Some(latest_ver) => {
            let new_tag = version_to_tag(&latest_ver);
            println!(
                "{}: updating {} → {}",
                manifest.name, installed_tag, new_tag
            );
            let subpath = source_subpath(&manifest.source).map(str::to_string);
            // Fetch BEFORE touching the existing installation (fail-safe)
            let fetched = fetcher.fetch(&url, &new_tag, subpath.as_deref()).await?;
            let new_source = rebuild_source(&manifest.source, &new_tag, &fetched);
            // Atomic swap: install to a staging dir, then rename over old
            atomic_upgrade(&manifest.name, fetched, new_source)?;
            println!("{}: updated to {}", manifest.name, new_tag);
        }
    }
    Ok(())
}

/// Install a new version atomically: write to a staging directory first,
/// then rename over the existing package directory. If anything fails before
/// the rename, the old package is untouched.
fn atomic_upgrade(name: &str, fetched: FetchedPackage, source: PackageSource) -> Result<()> {
    let staging_name = format!(".{name}.new");
    let staging_dir = packages_dir().join(&staging_name);
    let old_dir = package_dir(name);
    let backup_name = format!(".{name}.old");
    let backup_dir = packages_dir().join(&backup_name);

    // Clean up any stale staging/backup dirs from a previous interrupted update
    let _ = std::fs::remove_dir_all(&staging_dir);
    let _ = std::fs::remove_dir_all(&backup_dir);

    // Install into the staging directory (uses a fake name so install_package works)
    install_package(&staging_name, fetched, source)
        .with_context(|| format!("Failed to install new version of '{name}' to staging dir"))?;

    // Move old → backup, new → canonical. On failure, restore backup.
    std::fs::rename(&old_dir, &backup_dir)
        .with_context(|| format!("Failed to move old '{name}' to backup"))?;
    if let Err(e) = std::fs::rename(&staging_dir, &old_dir) {
        // Rename failed — try to restore the old version
        let _ = std::fs::rename(&backup_dir, &old_dir);
        let _ = std::fs::remove_dir_all(&staging_dir);
        return Err(e).with_context(|| {
            format!("Failed to move new '{name}' into place; old version restored")
        });
    }
    // Success — clean up backup
    let _ = std::fs::remove_dir_all(&backup_dir);

    // The manifest inside staging_dir was written with the staging name — rewrite it
    // with the real package name.
    fix_manifest_name(name)?;

    Ok(())
}

/// The manifest written during install has the staging name; overwrite the name field.
fn fix_manifest_name(name: &str) -> Result<()> {
    use harnx_core::config_paths::package_manifest_file;
    let manifest_path = package_manifest_file(name);
    let content = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("Failed to read manifest for '{name}'"))?;
    let mut manifest: harnx_core::package::PackageManifest = serde_yaml::from_str(&content)
        .with_context(|| format!("Failed to parse manifest for '{name}'"))?;
    manifest.name = name.to_string();
    let yaml = serde_yaml::to_string(&manifest)
        .with_context(|| format!("Failed to serialize manifest for '{name}'"))?;
    std::fs::write(&manifest_path, yaml)
        .with_context(|| format!("Failed to write manifest for '{name}'"))?;
    Ok(())
}

fn rebuild_source(old: &PackageSource, new_tag: &str, fetched: &FetchedPackage) -> PackageSource {
    match old {
        PackageSource::Git { url, subpath, .. } => PackageSource::Git {
            url: url.clone(),
            tag: new_tag.to_string(),
            commit: fetched.resolved_id.clone(),
            subpath: subpath.clone(),
        },
        PackageSource::Oci { url, subpath, .. } => PackageSource::Oci {
            url: url.clone(),
            tag: new_tag.to_string(),
            digest: fetched.resolved_id.clone(),
            subpath: subpath.clone(),
        },
    }
}
