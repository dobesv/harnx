//! Load-time script-tool catalog. No discovery-dependent acceptance decisions here;
//! declaration selection and dispatch must share the accepted view built on top of it.

use super::{tool_selection::is_bash_inner_tool_allowed, Config, BUILTIN_TOOLS};
mod view;
pub(crate) use view::script_tool_declaration;
pub(crate) use view::warn_collision;

use anyhow::{bail, Context, Result};
use harnx_core::config_paths::SCRIPT_TOOLS_DIR_NAME;
use harnx_core::script_tool::{validate_script_tool_name, ScriptToolDefinition};
use std::{collections::BTreeMap, fmt, path::Path, path::PathBuf, sync::Arc};

/// Location of a definition, retained after parsing for config and call diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptToolLocation {
    pub path: PathBuf,
    pub package: Option<String>,
    /// Zero-based index in global `script_tools:`; package files contain one entry.
    pub index: Option<usize>,
}

impl fmt::Display for ScriptToolLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.path.display())?;
        if let Some(index) = self.index {
            write!(f, " (script_tools[{index}])")?;
        }
        Ok(())
    }
}

/// Definition and compiled validator stay together and cannot be changed separately.
#[derive(Debug)]
pub struct ResolvedScriptTool {
    definition: ScriptToolDefinition,
    validator: jsonschema::Validator,
    location: ScriptToolLocation,
}

impl ResolvedScriptTool {
    // qual:allow(dry, boilerplate) reason: "Read-only access keeps the definition paired with its compiled validator."
    pub fn definition(&self) -> &ScriptToolDefinition {
        &self.definition
    }

    // qual:allow(dry, boilerplate) reason: "Callers may validate but cannot replace the validator independently of the definition."
    pub fn validator(&self) -> &jsonschema::Validator {
        &self.validator
    }

    // qual:allow(dry, boilerplate) reason: "The loaded source location must stay attached to this definition for diagnostics."
    pub fn location(&self) -> &ScriptToolLocation {
        &self.location
    }

    /// Validate only; JSON Schema `default` annotations never inject arguments.
    pub fn validate_arguments(&self, arguments: &serde_json::Value) -> Result<()> {
        self.validator.validate(arguments).map_err(|error| {
            anyhow::anyhow!(
                "script tool '{}' at {}: invalid arguments at '{}': {error}",
                self.definition.name,
                self.location,
                error.instance_path()
            )
        })
    }
}

/// Immutable, cheaply cloned catalog, sorted by declared name. Package names are
/// diagnostic context, never prefixes. Base-tool collision filtering belongs to
/// the shared accepted view, after base-tool discovery.
#[derive(Debug, Clone, Default)]
pub struct ScriptToolCatalog {
    entries: Arc<BTreeMap<String, Arc<ResolvedScriptTool>>>,
}

impl ScriptToolCatalog {
    pub fn get(&self, name: &str) -> Option<&Arc<ResolvedScriptTool>> {
        self.entries.get(name)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Arc<ResolvedScriptTool>> {
        self.entries.values()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Mutable only while loading. `finish` checks selectors against the complete
/// namespace so forward references and cross-package script calls also fail.
pub(super) struct ScriptToolCatalogBuilder {
    entries: BTreeMap<String, Arc<ResolvedScriptTool>>,
}

impl ScriptToolCatalogBuilder {
    pub(super) fn new(definitions: &[ScriptToolDefinition], path: &Path) -> Result<Self> {
        let mut builder = Self {
            entries: BTreeMap::new(),
        };
        for (index, definition) in definitions.iter().enumerate() {
            builder.insert(
                definition.clone(),
                ScriptToolLocation {
                    path: path.to_owned(),
                    package: None,
                    index: Some(index),
                },
            )?;
        }
        Ok(builder)
    }

    pub(super) fn load_package(&mut self, package_path: &Path, package: &str) -> Result<()> {
        let dir = package_path.join(SCRIPT_TOOLS_DIR_NAME);
        if !dir.exists() {
            return Ok(());
        }
        for path in Config::sorted_yaml_files(&dir)? {
            // Flat, regular *.yaml files only; the declared name wins over the stem.
            if !path.is_file() {
                continue;
            }
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to read script tool '{}'", path.display()))?;
            let definition = serde_yaml::from_str(&content)
                .with_context(|| format!("Failed to parse script tool '{}'", path.display()))?;
            self.insert(
                definition,
                ScriptToolLocation {
                    path,
                    package: Some(package.to_owned()),
                    index: None,
                },
            )?;
        }
        Ok(())
    }

    fn insert(
        &mut self,
        definition: ScriptToolDefinition,
        location: ScriptToolLocation,
    ) -> Result<()> {
        validate_definition_name(&definition.name, &location)?;
        self.validate_unique_name(&definition.name, &location)?;
        let validator = compile_schema(&definition, &location)?;
        self.entries.insert(
            definition.name.clone(),
            Arc::new(ResolvedScriptTool {
                definition,
                validator,
                location,
            }),
        );
        Ok(())
    }

    fn validate_unique_name(&self, name: &str, location: &ScriptToolLocation) -> Result<()> {
        if let Some(previous) = self.entries.get(name) {
            bail!(
                "duplicate script tool '{name}': {location} and {}",
                previous.location
            );
        }
        Ok(())
    }

    fn validate_selectors(&self, tool: &ResolvedScriptTool) -> Result<()> {
        let excluded = tool
            .definition
            .tools
            .iter()
            .map(|selector| selector.trim())
            .find(|selector| self.is_excluded_literal(selector));
        if let Some(selector) = excluded {
            bail!(
                "script tool '{}' at {}: tools selector '{selector}' names an excluded tool (bash, control tools and script tools cannot be called)",
                tool.definition.name,
                tool.location
            );
        }
        Ok(())
    }

    fn is_excluded_literal(&self, selector: &str) -> bool {
        // Globs are filtered at dispatch time, not rejected at load time.
        if selector.contains(['*', '?', '[', ']', '{', '}']) {
            return false;
        }
        let sanitized = harnx_core::package_namespace::sanitize_for_tool_name(selector);
        !is_bash_inner_tool_allowed(selector)
            || !is_bash_inner_tool_allowed(&sanitized)
            || self.entries.contains_key(selector)
            || self.entries.contains_key(&sanitized)
    }

    pub(super) fn finish(self) -> Result<ScriptToolCatalog> {
        for tool in self.entries.values() {
            self.validate_selectors(tool)?;
        }
        Ok(ScriptToolCatalog {
            entries: Arc::new(self.entries),
        })
    }
}

fn validate_definition_name(name: &str, location: &ScriptToolLocation) -> Result<()> {
    validate_script_tool_name(name)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("script tool '{name}' at {location}"))?;
    if !is_bash_inner_tool_allowed(name)
        || BUILTIN_TOOLS.iter().any(|(builtin, _)| *builtin == name)
    {
        bail!("script tool '{name}' at {location}: reserved tool name");
    }
    Ok(())
}

fn compile_schema(
    definition: &ScriptToolDefinition,
    location: &ScriptToolLocation,
) -> Result<jsonschema::Validator> {
    let schema = definition.parameters.as_value();
    if schema.get("type").and_then(serde_json::Value::as_str) != Some("object") {
        bail!(
            "script tool '{}' at {location}: parameters must declare top-level type: object",
            definition.name
        );
    }
    // jsonschema's default features are disabled: external HTTP/file references
    // fail compilation instead of accessing the network or host filesystem.
    jsonschema::validator_for(schema).with_context(|| {
        format!(
            "script tool '{}' at {location}: invalid parameters JSON Schema",
            definition.name
        )
    })
}

impl Config {
    /// Loaded once with the config. Runtime providers and declaration selection
    /// should derive their shared accepted view from this same catalog.
    pub fn script_tool_catalog(&self) -> &ScriptToolCatalog {
        &self.script_tool_catalog
    }

    /// Resolve and install a script-tool catalog from raw definitions.
    /// Config load does this for `config.yaml` definitions; embedders and
    /// tests that construct [`Config`] in memory use it to get the same
    /// validation and compiled validators.
    // qual:api
    pub fn set_script_tool_definitions(
        &mut self,
        definitions: &[ScriptToolDefinition],
    ) -> Result<()> {
        let builder = ScriptToolCatalogBuilder::new(definitions, Path::new("config.yaml"))?;
        self.script_tool_catalog = builder.finish()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
