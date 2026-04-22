//! Pure YAML-loaded configuration data.
//!
//! `ConfigData` owns the scalar payload of `config.yaml`. It is fully
//! serde-deserializable and has no runtime state, handles, or closures.
//! The `harnx` crate's `Config` type wraps `ConfigData` and adds runtime
//! fields (clients, mcp_manager, session, agent, rag, …).

use std::collections::HashMap;

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer};

use crate::agent_config::{deserialize_use_tools, normalize_toolset_value, ToolsetValue};
use crate::hooks::HooksConfig;

fn deserialize_toolsets<'de, D>(
    deserializer: D,
) -> std::result::Result<IndexMap<String, Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = IndexMap::<String, ToolsetValue>::deserialize(deserializer)?;
    Ok(values
        .into_iter()
        .map(|(key, value)| (key, normalize_toolset_value(value)))
        .collect())
}

/// Scalar YAML-deserialized fields from `config.yaml`.
///
/// This is the pure-data half of `harnx::Config`. It has no runtime state.
#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ConfigData {
    #[serde(rename(serialize = "model", deserialize = "model"))]
    #[serde(default)]
    pub model_id: String,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,

    pub dry_run: bool,
    pub stream: bool,
    pub save: bool,
    pub keybindings: String,
    pub editor: Option<String>,
    pub wrap: Option<String>,
    pub wrap_code: bool,

    pub tool_use: bool,
    #[serde(default)]
    #[serde(alias = "mapping_tools")]
    #[serde(deserialize_with = "deserialize_toolsets")]
    pub toolsets: IndexMap<String, Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_use_tools")]
    pub use_tools: Option<Vec<String>>,

    #[serde(alias = "repl_default_session")]
    pub tui_default_session: Option<String>,
    pub cmd_default_session: Option<String>,
    pub agent_default_session: Option<String>,

    pub save_session: Option<bool>,
    pub compress_threshold: usize,

    pub rag_embedding_model: Option<String>,
    pub rag_reranker_model: Option<String>,
    pub rag_top_k: usize,
    pub rag_chunk_size: Option<usize>,
    pub rag_chunk_overlap: Option<usize>,
    pub rag_template: Option<String>,

    #[serde(default)]
    pub document_loaders: HashMap<String, String>,

    pub highlight: bool,
    pub theme: Option<String>,

    pub serve_addr: Option<String>,
    pub user_agent: Option<String>,
    pub save_shell_history: bool,
    pub sync_models_url: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<HooksConfig>,
}

impl Default for ConfigData {
    fn default() -> Self {
        Self {
            model_id: Default::default(),
            temperature: None,
            top_p: None,

            dry_run: false,
            stream: true,
            save: false,
            keybindings: "emacs".into(),
            editor: None,
            wrap: None,
            wrap_code: false,

            tool_use: true,
            toolsets: Default::default(),
            use_tools: None,

            tui_default_session: None,
            cmd_default_session: None,
            agent_default_session: None,

            save_session: Some(true),
            compress_threshold: 180000,

            rag_embedding_model: None,
            rag_reranker_model: None,
            rag_top_k: 5,
            rag_chunk_size: None,
            rag_chunk_overlap: None,
            rag_template: None,

            document_loaders: Default::default(),

            highlight: true,
            theme: None,

            serve_addr: None,
            user_agent: None,
            save_shell_history: true,
            sync_models_url: None,

            hooks: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_historical_values() {
        // Guard against silent drift from the field-level defaults that
        // existed on `harnx::Config::default()` before the split.
        let d = ConfigData::default();
        assert_eq!(d.keybindings, "emacs");
        assert!(d.stream);
        assert!(!d.save);
        assert!(d.tool_use);
        assert!(d.highlight);
        assert!(d.save_shell_history);
        assert_eq!(d.save_session, Some(true));
        assert_eq!(d.compress_threshold, 180_000);
        assert_eq!(d.rag_top_k, 5);
    }

    #[test]
    fn empty_yaml_deserializes_to_defaults() {
        // `#[serde(default)]` on the struct + on the individual fields means
        // an empty YAML document must round-trip through Default.
        let got: ConfigData = serde_yaml::from_str("{}").unwrap();
        let default = ConfigData::default();
        assert_eq!(got.keybindings, default.keybindings);
        assert_eq!(got.stream, default.stream);
        assert_eq!(got.tool_use, default.tool_use);
        assert_eq!(got.save_session, default.save_session);
        assert_eq!(got.compress_threshold, default.compress_threshold);
        assert_eq!(got.rag_top_k, default.rag_top_k);
        assert_eq!(got.highlight, default.highlight);
    }

    #[test]
    fn golden_yaml_populates_expected_fields() {
        let yaml = r#"
model: claude-sonnet-4-6
temperature: 0.7
stream: false
save: true
keybindings: vi
compress_threshold: 100000
rag_top_k: 3
highlight: false
hooks:
  entries: []
"#;
        let got: ConfigData = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(got.model_id, "claude-sonnet-4-6");
        assert_eq!(got.temperature, Some(0.7));
        assert!(!got.stream);
        assert!(got.save);
        assert_eq!(got.keybindings, "vi");
        assert_eq!(got.compress_threshold, 100_000);
        assert_eq!(got.rag_top_k, 3);
        assert!(!got.highlight);
        assert!(got.hooks.is_some());
    }

    #[test]
    fn repl_default_session_alias_still_works() {
        let yaml = "repl_default_session: legacy_session\n";
        let got: ConfigData = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(got.tui_default_session.as_deref(), Some("legacy_session"));
    }

    #[test]
    fn mapping_tools_alias_for_toolsets() {
        let yaml = "mapping_tools:\n  demo: [fs_read, fs_write]\n";
        let got: ConfigData = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            got.toolsets.get("demo"),
            Some(&vec!["fs_read".to_string(), "fs_write".to_string()])
        );
    }
}
