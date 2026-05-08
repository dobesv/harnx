use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::model::RequestPatch;

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

/// Written by harnx-pkg at install time into packages/<name>/manifest.yaml.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PackageManifest {
    pub name: String,
    pub source: PackageSource,
    pub installed_at: String,
}

/// Optional metadata provided by package itself in package.yaml.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct PackageMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harnx_min_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Agent patch entry — overrides for matched agent's config fields.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct AgentPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_models: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
}

/// Client patch entry — typed overrides for a matched client's config.
///
/// Fields mirror the common subset of all provider configs. Provider-specific
/// fields (Bedrock credentials, VertexAI project_id, etc.) are intentionally
/// omitted — those should be configured at the system level, not in a package patch.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ClientPatch {
    /// Override the API key for this client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Override the API base URL for this client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base: Option<String>,
    /// Prepend lines to the system prompt for all requests through this client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_prefix: Option<Vec<String>>,
    /// Override request-body patches (merged into the client's existing `patch` config).
    /// Contains per-endpoint patches: `chat_completions`, `embeddings`, `rerank`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<RequestPatch>,
}

/// Contents of packages/<pkg>.patch.yaml — local customization of installed package.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct PackagePatch {
    /// Map of agent name regexp → patch to apply.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub agents: IndexMap<String, AgentPatch>,
    /// Map of client name regexp → patch to apply.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub clients: IndexMap<String, ClientPatch>,
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
                url: "ghcr.io/example/package".to_string(),
                tag: "1.0.0".to_string(),
                digest: "sha256:deadbeef".to_string(),
                subpath: Some("path".to_string()),
            },
            installed_at: "2026-05-07T00:00:00Z".to_string(),
        };

        let yaml = serde_yaml::to_string(&manifest).unwrap();
        let roundtrip: PackageManifest = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(roundtrip, manifest);
    }

    #[test]
    fn test_metadata_serde_partial() {
        let yaml = "name: Example Package\ndescription: Test package\n";
        let metadata: PackageMetadata = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(metadata.name.as_deref(), Some("Example Package"));
        assert_eq!(metadata.description.as_deref(), Some("Test package"));
        assert_eq!(metadata.harnx_min_version, None);
        assert_eq!(metadata.homepage, None);
        assert_eq!(metadata.license, None);
        assert_eq!(metadata.version, None);
    }

    #[test]
    fn test_patch_serde_roundtrip() {
        let mut agents = IndexMap::new();
        agents.insert(
            ".*".to_string(),
            AgentPatch {
                model: Some("claude".to_string()),
                ..Default::default()
            },
        );

        let client_patch = ClientPatch {
            api_key: Some("sk-test".to_string()),
            api_base: Some("https://example.invalid/v1".to_string()),
            system_prompt_prefix: Some(vec!["Always respond in haiku.".to_string()]),
            patch: None,
        };

        let mut clients = IndexMap::new();
        clients.insert(".*".to_string(), client_patch);

        let patch = PackagePatch { agents, clients };

        let yaml = serde_yaml::to_string(&patch).unwrap();
        let roundtrip: PackagePatch = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(roundtrip, patch);
    }

    #[test]
    fn test_client_patch_with_request_patch_roundtrip() {
        use crate::model::RequestPatch;
        use indexmap::IndexMap;
        use serde_json::json;

        let mut chat = IndexMap::new();
        chat.insert("max_tokens".to_string(), json!(2048));

        let client_patch = ClientPatch {
            api_key: None,
            api_base: None,
            system_prompt_prefix: None,
            patch: Some(RequestPatch {
                chat_completions: Some(chat),
                embeddings: None,
                rerank: None,
            }),
        };

        let yaml = serde_yaml::to_string(&client_patch).unwrap();
        let roundtrip: ClientPatch = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(roundtrip, client_patch);
    }
}
