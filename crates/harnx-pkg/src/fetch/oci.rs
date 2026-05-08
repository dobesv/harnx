use std::fs;
use std::io::Cursor;
use std::path::Path;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use oci_client::{
    client::{ClientConfig, ClientProtocol},
    manifest,
    secrets::RegistryAuth,
    Client, Reference,
};

use super::{FetchedPackage, PackageFetcher};
use crate::semver_util::parse_semver_tag;

pub struct OciFetcher;

#[async_trait]
impl PackageFetcher for OciFetcher {
    async fn fetch(&self, url: &str, tag: &str, subpath: Option<&str>) -> Result<FetchedPackage> {
        parse_semver_tag(tag)?;

        let (registry, repository) = parse_oci_url(url)?;
        let reference: Reference = format!("{registry}/{repository}:{tag}")
            .parse()
            .with_context(|| format!("invalid OCI reference for '{url}:{tag}'"))?;

        let client = oci_client_for_registry(&registry);
        let auth = RegistryAuth::Anonymous;
        let accepted_media_types = [
            manifest::IMAGE_LAYER_MEDIA_TYPE,
            manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE,
            manifest::IMAGE_DOCKER_LAYER_TAR_MEDIA_TYPE,
            manifest::IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE,
            manifest::IMAGE_LAYER_NONDISTRIBUTABLE_MEDIA_TYPE,
            manifest::IMAGE_LAYER_NONDISTRIBUTABLE_GZIP_MEDIA_TYPE,
        ];

        let image = client
            .pull(&reference, &auth, accepted_media_types.to_vec())
            .await
            .with_context(|| format!("failed to pull OCI artifact '{reference}'"))?;

        let resolved_id = image.digest.clone().ok_or_else(|| {
            anyhow::anyhow!("registry did not return a manifest digest for '{reference}'")
        })?;

        let extracted_dir =
            tempfile::TempDir::new().context("failed to create extraction temp dir")?;
        unpack_layers(extracted_dir.path(), &image.layers)?;

        let final_dir = if let Some(sp) = subpath {
            let src = extracted_dir.path().join(sp);
            if !src.is_dir() {
                bail!("subpath '{sp}' does not exist in OCI package");
            }
            let dest = tempfile::TempDir::new().context("failed to create subpath temp dir")?;
            copy_dir_recursive(&src, dest.path())?;
            dest
        } else {
            let dest = tempfile::TempDir::new().context("failed to create final temp dir")?;
            copy_dir_recursive(extracted_dir.path(), dest.path())?;
            dest
        };

        Ok(FetchedPackage {
            dir: final_dir,
            resolved_id,
            tag: tag.to_string(),
        })
    }

    async fn list_tags(&self, url: &str) -> Result<Vec<semver::Version>> {
        let (registry, repository) = parse_oci_url(url)?;
        let reference: Reference = format!("{registry}/{repository}:latest")
            .parse()
            .with_context(|| format!("invalid OCI reference for '{url}'"))?;

        let client = oci_client_for_registry(&registry);
        let response = client
            .list_tags(&reference, &RegistryAuth::Anonymous, None, None)
            .await
            .with_context(|| format!("failed to list OCI tags for '{url}'"))?;

        let mut versions = response
            .tags
            .into_iter()
            .filter_map(|tag| parse_semver_tag(&tag).ok())
            .collect::<Vec<_>>();
        versions.sort();
        Ok(versions)
    }
}

/// Parse an OCI URL into (registry, repository).
/// Accepts:
///   "oci://ghcr.io/owner/repo"  → ("ghcr.io", "owner/repo")
///   "ghcr.io/owner/repo"        → ("ghcr.io", "owner/repo")
///   "localhost:5000/owner/repo" → ("localhost:5000", "owner/repo")
fn parse_oci_url(url: &str) -> Result<(String, String)> {
    let stripped = url.strip_prefix("oci://").unwrap_or(url);
    if let Some(slash) = stripped.find('/') {
        let registry = stripped[..slash].to_string();
        let repository = stripped[slash + 1..].to_string();
        if registry.is_empty() || repository.is_empty() {
            bail!("Invalid OCI URL: '{url}' — expected registry/repository");
        }
        Ok((registry, repository))
    } else {
        bail!("Invalid OCI URL: '{url}' — expected registry/repository")
    }
}

fn oci_client_for_registry(registry: &str) -> Client {
    Client::new(ClientConfig {
        protocol: ClientProtocol::HttpsExcept(vec![registry.to_string()]),
        ..Default::default()
    })
}

fn unpack_layers(dst: &Path, layers: &[oci_client::client::ImageLayer]) -> Result<()> {
    for layer in layers {
        unpack_layer(dst, &layer.data, &layer.media_type).with_context(|| {
            format!(
                "failed to unpack OCI layer with media type '{}'",
                layer.media_type
            )
        })?;
    }
    Ok(())
}

fn unpack_layer(dst: &Path, data: &[u8], media_type: &str) -> Result<()> {
    match media_type {
        manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE
        | manifest::IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE
        | manifest::IMAGE_LAYER_NONDISTRIBUTABLE_GZIP_MEDIA_TYPE => {
            let decoder = flate2::read::GzDecoder::new(Cursor::new(data));
            let mut archive = tar::Archive::new(decoder);
            archive.unpack(dst)?;
        }
        manifest::IMAGE_LAYER_MEDIA_TYPE
        | manifest::IMAGE_DOCKER_LAYER_TAR_MEDIA_TYPE
        | manifest::IMAGE_LAYER_NONDISTRIBUTABLE_MEDIA_TYPE => {
            let mut archive = tar::Archive::new(Cursor::new(data));
            archive.unpack(dst)?;
        }
        _ => {
            if is_gzip(data) {
                let decoder = flate2::read::GzDecoder::new(Cursor::new(data));
                let mut archive = tar::Archive::new(decoder);
                archive.unpack(dst)?;
            } else {
                let mut archive = tar::Archive::new(Cursor::new(data));
                archive.unpack(dst)?;
            }
        }
    }
    Ok(())
}

fn is_gzip(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            fs::create_dir_all(&dst_path)?;
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            if let Some(parent) = dst_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&src_path, &dst_path)?;
        } else if file_type.is_symlink() {
            let target = fs::read_link(&src_path)?;
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&target, &dst_path)?;
            }
            #[cfg(not(unix))]
            {
                let resolved = src_path.parent().unwrap_or(src).join(target);
                fs::copy(resolved, &dst_path)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_oci_url;

    #[test]
    fn parse_url_with_scheme() {
        let (registry, repo) = parse_oci_url("oci://ghcr.io/owner/repo").unwrap();
        assert_eq!(registry, "ghcr.io");
        assert_eq!(repo, "owner/repo");
    }

    #[test]
    fn parse_url_with_port() {
        let (registry, repo) = parse_oci_url("localhost:5000/owner/repo").unwrap();
        assert_eq!(registry, "localhost:5000");
        assert_eq!(repo, "owner/repo");
    }

    #[test]
    fn parse_url_rejects_missing_repo() {
        assert!(parse_oci_url("oci://ghcr.io").is_err());
    }
}
