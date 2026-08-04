use crate::fetch::FetchedPackage;
use anyhow::{bail, Context, Result};
use harnx_core::config_paths::{
    package_dir, package_manifest_file, package_patch_file, packages_dir,
};
use harnx_core::package::{PackageManifest, PackagePatch, PackageSource};
use std::fs;
use std::path::Path;

/// Install a fetched package into config packages directory.
pub fn install_package(
    name: &str,
    fetched: FetchedPackage,
    source: PackageSource,
) -> Result<PackageManifest> {
    let target_dir = package_dir(name);

    if target_dir.exists() {
        bail!("package '{name}' is already installed; use `harnx-pkg update` to upgrade");
    }

    fs::create_dir_all(&target_dir).with_context(|| {
        format!(
            "Failed to create package directory '{}'",
            target_dir.display()
        )
    })?;

    copy_package_files(fetched.dir.path(), &target_dir)?;

    print_install_summary(name, &target_dir, &source, &fetched.tag);

    let manifest = PackageManifest {
        name: name.to_string(),
        source,
        installed_at: chrono::Utc::now().to_rfc3339(),
    };
    let yaml = serde_yaml::to_string(&manifest).context("Failed to serialize manifest")?;
    fs::write(package_manifest_file(name), &yaml)
        .with_context(|| format!("Failed to write manifest for package '{name}'"))?;

    Ok(manifest)
}

/// Remove installed package directory.
pub fn remove_package(name: &str) -> Result<()> {
    load_manifest(name).with_context(|| format!("Package '{name}' is not installed"))?;

    let dir = package_dir(name);
    fs::remove_dir_all(&dir)
        .with_context(|| format!("Failed to remove package directory '{}'", dir.display()))?;

    println!("Removed package '{name}'.");
    println!("Note: session transcripts referencing '{name}' agents are preserved.");
    Ok(())
}

/// List all installed packages.
pub fn list_installed_packages() -> Result<Vec<PackageManifest>> {
    let dir = packages_dir();
    if !dir.exists() {
        return Ok(vec![]);
    }

    let mut manifests = Vec::new();
    for entry in fs::read_dir(&dir)
        .with_context(|| format!("Failed to read packages dir '{}'", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let pkg_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if pkg_name.starts_with('.') {
            continue;
        }
        match load_manifest(&pkg_name) {
            Ok(m) => manifests.push(m),
            Err(e) => log::warn!("Skipping package '{pkg_name}': {e}"),
        }
    }

    manifests.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(manifests)
}

/// Load manifest for single installed package.
pub fn load_manifest(name: &str) -> Result<PackageManifest> {
    let path = package_manifest_file(name);
    let content = fs::read_to_string(&path).with_context(|| {
        format!(
            "Package '{name}' not found (no manifest at '{}')",
            path.display()
        )
    })?;
    serde_yaml::from_str(&content)
        .with_context(|| format!("Failed to parse manifest for package '{name}'"))
}

/// Load optional patch file for package.
pub fn load_patch(name: &str) -> Result<Option<PackagePatch>> {
    let path = package_patch_file(name);
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read patch file '{}'", path.display()))?;
    let patch = serde_yaml::from_str(&content)
        .with_context(|| format!("Failed to parse patch file for package '{name}'"))?;
    Ok(Some(patch))
}

fn copy_package_files(src: &Path, dst: &Path) -> Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let src_path = entry.path();

        if name == "manifest.yaml" && src_path.parent() == Some(src) {
            continue;
        }
        if name == ".git" {
            continue;
        }

        let dst_path = dst.join(&name);
        if src_path.is_dir() {
            fs::create_dir_all(&dst_path)?;
            copy_package_files(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

fn print_install_summary(name: &str, pkg_dir: &Path, source: &PackageSource, tag: &str) {
    let (source_url, source_type) = match source {
        PackageSource::Git { url, .. } => (url.as_str(), "git"),
        PackageSource::Oci { url, .. } => (url.as_str(), "oci"),
    };
    println!("Installing package '{name}' from {source_type} {source_url} @ {tag}");

    let agents_dir = pkg_dir.join("agents");
    if agents_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&agents_dir) {
            let agents: Vec<_> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md"))
                .filter_map(|e| {
                    e.path()
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(str::to_string)
                })
                .collect();
            if !agents.is_empty() {
                println!("  Agents: {}", agents.join(", "));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    fn with_config_dir<F: FnOnce(&std::path::Path) -> R, R>(f: F) -> R {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        unsafe { std::env::set_var("HARNX_CONFIG_DIR", tmp.path()) };
        let result = f(tmp.path());
        unsafe { std::env::remove_var("HARNX_CONFIG_DIR") };
        result
    }

    fn make_source() -> PackageSource {
        PackageSource::Git {
            url: "file:///fake/repo".to_string(),
            tag: "v1.0.0".to_string(),
            commit: "abc123".to_string(),
            subpath: None,
        }
    }

    fn make_fetched(files: &[(&str, &str)]) -> crate::fetch::FetchedPackage {
        let dir = tempfile::TempDir::new().unwrap();
        for (path, content) in files {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, content).unwrap();
        }
        crate::fetch::FetchedPackage {
            dir,
            resolved_id: "abc123".to_string(),
            tag: "v1.0.0".to_string(),
        }
    }

    #[test]
    fn test_install_writes_manifest() {
        with_config_dir(|_| {
            let fetched = make_fetched(&[("agents/foo.md", "agent content")]);
            let manifest = install_package("testpkg", fetched, make_source()).unwrap();
            assert_eq!(manifest.name, "testpkg");
            let manifest_path = package_manifest_file("testpkg");
            assert!(manifest_path.exists());
            let loaded: PackageManifest =
                serde_yaml::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
            assert_eq!(loaded.name, "testpkg");
        });
    }

    #[test]
    fn test_install_copies_files() {
        with_config_dir(|_| {
            let fetched = make_fetched(&[("agents/foo.md", "# Foo agent")]);
            install_package("testpkg2", fetched, make_source()).unwrap();
            assert!(package_dir("testpkg2").join("agents/foo.md").exists());
        });
    }

    #[test]
    fn test_install_skips_upstream_manifest() {
        with_config_dir(|_| {
            let fetched = make_fetched(&[
                ("agents/foo.md", "agent"),
                ("manifest.yaml", "name: upstream-should-be-ignored"),
            ]);
            install_package("testpkg3", fetched, make_source()).unwrap();
            let manifest_path = package_manifest_file("testpkg3");
            let content = std::fs::read_to_string(&manifest_path).unwrap();
            assert!(content.contains("testpkg3"));
            assert!(!content.contains("upstream-should-be-ignored"));
        });
    }

    #[test]
    fn test_install_rejects_duplicate() {
        with_config_dir(|_| {
            let fetched1 = make_fetched(&[("agents/foo.md", "agent")]);
            install_package("dupl", fetched1, make_source()).unwrap();
            let fetched2 = make_fetched(&[("agents/bar.md", "agent")]);
            let result = install_package("dupl", fetched2, make_source());
            assert!(result.is_err(), "Expected error for duplicate install");
        });
    }

    #[test]
    fn test_remove_cleans_dir() {
        with_config_dir(|_| {
            let fetched = make_fetched(&[("agents/foo.md", "agent")]);
            install_package("rmpkg", fetched, make_source()).unwrap();
            assert!(package_dir("rmpkg").exists());
            remove_package("rmpkg").unwrap();
            assert!(!package_dir("rmpkg").exists());
        });
    }

    #[test]
    fn test_list_returns_installed() {
        with_config_dir(|_| {
            let f1 = make_fetched(&[("agents/a.md", "a")]);
            install_package("alpha", f1, make_source()).unwrap();
            let f2 = make_fetched(&[("agents/b.md", "b")]);
            install_package("beta", f2, make_source()).unwrap();
            let list = list_installed_packages().unwrap();
            let names: Vec<_> = list.iter().map(|m| m.name.as_str()).collect();
            assert!(names.contains(&"alpha"));
            assert!(names.contains(&"beta"));
        });
    }

    #[test]
    fn test_load_patch_absent() {
        with_config_dir(|_| {
            std::fs::create_dir_all(packages_dir()).unwrap();
            let result = load_patch("nonexistent").unwrap();
            assert!(result.is_none());
        });
    }
}
