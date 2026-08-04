use serde::{Deserialize, Serialize};

/// Source from which a package was installed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PackageSource {
    Git {
        url: String,
        tag: String,
        commit: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subpath: Option<String>,
    },
    Oci {
        url: String,
        tag: String,
        digest: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subpath: Option<String>,
    },
}

/// Metadata about an installed package, persisted alongside the package files.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PackageManifest {
    pub name: String,
    pub source: PackageSource,
    /// ISO 8601 timestamp when the package was installed.
    pub installed_at: String,
}

/// Contents of packages/<pkg>/package.yaml — the package's metadata.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct PackageMetadata {
    /// Human-readable description of the package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// SPDX license identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Contents of packages/<pkg>.patch.yaml — local customization of installed package.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct PackagePatch {
    /// JQ expressions for patching agent configs.
    #[serde(default)]
    pub agents: Vec<String>,
    /// JQ expressions for patching client configs.
    #[serde(default)]
    pub clients: Vec<String>,
    /// JQ expressions for patching tool server configs.
    #[serde(default)]
    pub tool_servers: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_serde_git() {
        let manifest = PackageManifest {
            name: "example".to_string(),
            source: PackageSource::Git {
                url: "https://github.com/example/repo.git".to_string(),
                tag: "v1.2.3".to_string(),
                commit: "abc123".to_string(),
                subpath: None,
            },
            installed_at: "2026-05-07T00:00:00Z".to_string(),
        };

        let yaml = serde_yaml::to_string(&manifest).unwrap();
        let roundtrip: PackageManifest = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(roundtrip, manifest);
    }

    #[test]
    fn test_manifest_serde_oci() {
        let manifest = PackageManifest {
            name: "example".to_string(),
            source: PackageSource::Oci {
                url: "oci://registry.example.com/example".to_string(),
                tag: "v1.2.3".to_string(),
                digest: "sha256:abc123".to_string(),
                subpath: None,
            },
            installed_at: "2026-05-07T00:00:00Z".to_string(),
        };

        let yaml = serde_yaml::to_string(&manifest).unwrap();
        let roundtrip: PackageManifest = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(roundtrip, manifest);
    }

    #[test]
    fn test_package_patch_serde_roundtrip() {
        let patch = PackagePatch {
            agents: vec![".model = \"claude\"".to_string()],
            clients: vec![".api_key = \"sk-test\"".to_string()],
            tool_servers: vec![".enabled = false".to_string()],
        };

        let yaml = serde_yaml::to_string(&patch).unwrap();
        let roundtrip: PackagePatch = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(roundtrip, patch);
    }

    #[test]
    fn test_package_patch_empty_roundtrip() {
        let patch = PackagePatch::default();

        let yaml = serde_yaml::to_string(&patch).unwrap();
        let roundtrip: PackagePatch = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(roundtrip, patch);
    }

    #[test]
    fn test_package_patch_partial() {
        let patch = PackagePatch {
            agents: vec![".model = \"claude\"".to_string()],
            clients: Vec::new(),
            tool_servers: Vec::new(),
        };

        let yaml = serde_yaml::to_string(&patch).unwrap();
        let roundtrip: PackagePatch = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(roundtrip, patch);
    }
}
