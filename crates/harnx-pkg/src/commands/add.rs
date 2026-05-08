use crate::cli::AddArgs;
use crate::fetch::{git::GitFetcher, oci::OciFetcher, FetchedPackage, PackageFetcher};
use crate::install::install_package;
use crate::semver_util::parse_semver_tag;
use anyhow::{bail, Context, Result};
use harnx_core::package::PackageSource;

pub async fn run(args: &AddArgs) -> Result<()> {
    // Validate tag upfront
    parse_semver_tag(&args.tag).with_context(|| {
        format!(
            "'{}' is not a valid semver tag (expected v<major>.<minor>.<patch>)",
            args.tag
        )
    })?;

    let is_oci = is_oci_url(&args.url);
    let fetcher: Box<dyn PackageFetcher> = fetcher_for_url(&args.url);
    let name = args
        .name
        .clone()
        .unwrap_or_else(|| infer_package_name(&args.url));
    validate_package_name(&name)?;

    println!("Fetching '{name}' @ {} from {}", args.tag, args.url);
    let fetched = fetcher
        .fetch(&args.url, &args.tag, args.subpath.as_deref())
        .await?;
    let source = build_source(
        &args.url,
        &args.tag,
        &fetched,
        args.subpath.as_deref(),
        is_oci,
    );
    install_package(&name, fetched, source)?;
    println!("✓ Package '{name}' installed successfully.");
    Ok(())
}

/// Returns true if the URL points to an OCI registry.
pub fn is_oci_url(url: &str) -> bool {
    if url.starts_with("oci://") {
        return true;
    }
    // If it doesn't look like a git URL, treat as OCI
    !url.ends_with(".git")
        && !url.contains("github.com")
        && !url.contains("gitlab.com")
        && !url.contains("bitbucket")
        && !url.starts_with("file://")
        && !url.starts_with("git://")
        && !url.starts_with("ssh://")
}

pub fn fetcher_for_url(url: &str) -> Box<dyn PackageFetcher> {
    if is_oci_url(url) {
        return Box::new(OciFetcher);
    }
    if url.ends_with(".git")
        || url.contains("github.com")
        || url.contains("gitlab.com")
        || url.contains("bitbucket")
        || url.starts_with("file://")
        || url.starts_with("git://")
        || url.starts_with("ssh://")
    {
        return Box::new(GitFetcher);
    }
    // Default: try git
    log::warn!("Cannot determine source type from URL '{url}', trying git");
    Box::new(GitFetcher)
}

pub fn infer_package_name(url: &str) -> String {
    // Strip scheme
    let stripped = url
        .strip_prefix("oci://")
        .or_else(|| url.strip_prefix("https://"))
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("git://"))
        .or_else(|| url.strip_prefix("ssh://"))
        .or_else(|| url.strip_prefix("file://"))
        .unwrap_or(url);
    // Take the last path component, strip .git suffix
    let last = stripped
        .split('/')
        .rfind(|s| !s.is_empty())
        .unwrap_or("package");
    last.strip_suffix(".git")
        .unwrap_or(last)
        .to_ascii_lowercase()
}

pub fn validate_package_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Package name cannot be empty");
    }
    if !name
        .chars()
        .next()
        .map(|c| c.is_ascii_alphanumeric())
        .unwrap_or(false)
    {
        bail!("Package name '{name}' must start with a letter or digit");
    }
    for ch in name.chars() {
        if !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_' {
            bail!(
                "Package name '{name}' contains invalid character '{ch}' (allowed: a-z, 0-9, -, _)"
            );
        }
    }
    Ok(())
}

/// Build the correct PackageSource variant based on whether this is git or OCI.
fn build_source(
    url: &str,
    tag: &str,
    fetched: &FetchedPackage,
    subpath: Option<&str>,
    is_oci: bool,
) -> PackageSource {
    if is_oci {
        PackageSource::Oci {
            url: url.to_string(),
            tag: tag.to_string(),
            digest: fetched.resolved_id.clone(),
            subpath: subpath.map(str::to_string),
        }
    } else {
        PackageSource::Git {
            url: url.to_string(),
            tag: tag.to_string(),
            commit: fetched.resolved_id.clone(),
            subpath: subpath.map(str::to_string),
        }
    }
}

// Keep backward-compatible name for internal use
#[allow(dead_code)]
fn build_source_git(
    url: &str,
    tag: &str,
    fetched: &FetchedPackage,
    subpath: Option<&str>,
) -> PackageSource {
    build_source(url, tag, fetched, subpath, false)
}
