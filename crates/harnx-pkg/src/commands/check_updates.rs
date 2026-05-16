use crate::cli::CheckForUpdatesArgs;
use crate::fetch::{git::GitFetcher, oci::OciFetcher, PackageFetcher};
use crate::install::{list_installed_packages, load_manifest};
use crate::semver_util::{find_latest_tag, is_newer_version, parse_semver_tag, version_to_tag};
use anyhow::Result;
use harnx_core::package::PackageSource;

pub async fn run(args: &CheckForUpdatesArgs) -> Result<()> {
    let packages = if let Some(name) = &args.name {
        vec![load_manifest(name)?]
    } else {
        list_installed_packages()?
    };

    for manifest in &packages {
        let fetcher = fetcher_for_source(&manifest.source).await?;
        let url = source_url(&manifest.source);
        let installed_tag = source_tag(&manifest.source);
        let installed_ver = parse_semver_tag(installed_tag)?;

        let versions = fetcher.list_tags(&url).await?;
        let tag_strings: Vec<String> = versions.iter().map(version_to_tag).collect();
        let latest = find_latest_tag(&tag_strings);

        match latest {
            Some(v) if is_newer_version(&installed_ver, &v) => {
                println!(
                    "{}: update available {} → v{}",
                    manifest.name, installed_tag, v
                );
            }
            _ => {
                println!("{}: up to date ({})", manifest.name, installed_tag);
            }
        }
    }
    Ok(())
}

pub async fn fetcher_for_source(source: &PackageSource) -> Result<Box<dyn PackageFetcher>> {
    match source {
        PackageSource::Git { .. } => Ok(Box::new(GitFetcher)),
        PackageSource::Oci { url, .. } => {
            let auth = crate::credentials::resolve_oci_auth(url).await?;
            Ok(Box::new(OciFetcher::with_auth(auth)))
        }
    }
}

pub fn source_url(source: &PackageSource) -> String {
    match source {
        PackageSource::Git { url, .. } => url.clone(),
        PackageSource::Oci { url, .. } => url.clone(),
    }
}

pub fn source_tag(source: &PackageSource) -> &str {
    match source {
        PackageSource::Git { tag, .. } => tag.as_str(),
        PackageSource::Oci { tag, .. } => tag.as_str(),
    }
}

pub fn source_subpath(source: &PackageSource) -> Option<&str> {
    match source {
        PackageSource::Git { subpath, .. } => subpath.as_deref(),
        PackageSource::Oci { subpath, .. } => subpath.as_deref(),
    }
}
