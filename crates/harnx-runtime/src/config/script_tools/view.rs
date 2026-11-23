//! The accepted view of the script-tool catalog (decision 12): which script
//! tools may be declared and dispatched this round, and which base tools
//! their `tools:` selectors may call. A tool round computes the view once
//! and threads it to both the declaration snapshot and the
//! `ScriptToolProvider`, so the advertised schema and the executed body
//! always correspond.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use harnx_core::script_tool::ScriptToolDefinition;
use harnx_core::tool::ToolDeclaration;

use super::super::{
    tool_name_selector, tool_selection::is_bash_inner_tool_allowed, Config, ToolSelector,
    BUILTIN_TOOLS,
};
use super::ResolvedScriptTool;

impl Config {
    /// The shared accepted view (decision 12): catalog entries minus
    /// base-tool name collisions. A tool round computes this view once,
    /// under the same config read as its declaration selection, and threads
    /// it to both the declarations and the `ScriptToolProvider`, so the
    /// advertised schema and the executed body always correspond and a
    /// mid-round discovery refresh cannot desynchronize them.
    ///
    /// A script tool whose name collides with a native/MCP/tool-server tool is
    /// not accepted: the original stays callable and is never shadowed.
    pub fn accepted_script_tools(&self) -> Vec<Arc<ResolvedScriptTool>> {
        self.accepted_script_tools_against(&self.base_tool_names())
    }

    /// The accepted view computed against one captured base-name set — the
    /// config's own NATS declarations plus the round's resolved provider
    /// declarations. A tool round passes the same captured set here and to
    /// the declaration selection, so the view and the advertisements cannot
    /// disagree across a discovery refresh.
    pub(crate) fn accepted_script_tools_against(
        &self,
        base_names: &HashSet<String>,
    ) -> Vec<Arc<ResolvedScriptTool>> {
        if self.script_tool_catalog.is_empty() {
            return Vec::new();
        }
        self.script_tool_catalog
            .iter()
            .filter(|tool| {
                let name = tool.definition().name.as_str();
                if base_names.contains(name) {
                    warn_collision(tool);
                    return false;
                }
                true
            })
            .cloned()
            .collect()
    }

    /// Declarations for the tools one script tool's `tools:` selectors may
    /// call, resolved against the tools the base providers expose. Control
    /// tools and every script tool are excluded unconditionally, so a glob
    /// that matches them skips them silently (decision 5). The resolution is
    /// base-only (C7): an empty accepted view keeps script-tool declarations
    /// from being built just to be filtered out, and it cannot evict a base
    /// tool that happens to share a disabled script tool's name.
    pub(crate) fn script_tool_inner_declarations(
        &self,
        selectors: &[String],
        package: Option<&str>,
    ) -> Vec<ToolDeclaration> {
        if selectors.is_empty() {
            return Vec::new();
        }
        self.select_tools_for_package_with_script_tools(selectors, package, &[])
            .into_iter()
            .filter(|tool| is_bash_inner_tool_allowed(&tool.name))
            .collect()
    }

    /// The `tools:` selectors of every accepted script tool the given
    /// selectors select (literal name, glob, or toolset member), for managed
    /// tool-server startup (decision 13). Unselected script tools contribute
    /// nothing.
    ///
    /// Matching mirrors `collect_selected_tool_names`: a dotted selector
    /// such as `issue.brief` also matches the sanitized script-tool name
    /// `issue_brief` at runtime, so startup must resolve it the same way or
    /// the selected script's inner servers would never start.
    pub fn script_tool_inner_selectors_for(&self, selectors: &[String]) -> Vec<String> {
        let accepted = self.accepted_script_tools();
        if accepted.is_empty() || selectors.is_empty() {
            return Vec::new();
        }
        let expanded = self.expanded_tool_selectors(&selectors.join(","));
        let compiled: Vec<(ToolSelector, Option<ToolSelector>)> = expanded
            .iter()
            .map(|selector| {
                let trimmed = selector.trim();
                let sanitized = harnx_core::package_namespace::sanitize_for_tool_name(trimmed);
                (
                    tool_name_selector(trimmed),
                    (sanitized != trimmed).then(|| tool_name_selector(&sanitized)),
                )
            })
            .collect();
        accepted
            .iter()
            .filter(|tool| {
                let name = tool.definition().name.as_str();
                expanded.iter().any(|selector| selector.trim() == name)
                    || compiled.iter().any(|(selector, sanitized)| {
                        selector.is_match(name)
                            || sanitized.as_ref().is_some_and(|s| s.is_match(name))
                    })
            })
            .flat_map(|tool| tool.definition().tools.iter().cloned())
            .collect()
    }

    /// Names that own their tool identity ahead of script tools: discovered
    /// MCP tools, NATS tool declarations, the in-process builtins, and every
    /// name the last refreshed provider routes — raw aliases included, which
    /// no declaration list carries. Folding the route-name cache in keeps
    /// `select_tools` (the model-visible tool list) and the round snapshot
    /// in agreement about what a script tool may claim.
    fn base_tool_names(&self) -> HashSet<String> {
        let mut names = self.base_tool_names_with(&self.nats_tool_declarations.read(), &[]);
        names.extend(self.nats_served_tool_names.read().iter().cloned());
        names
    }

    /// Same identity set, computed from one captured NATS declaration list
    /// and the round's resolved provider declarations — the names the
    /// round's advertisements can carry. Provider-served names beyond
    /// these declarations (raw aliases, unselected tools) are excluded at
    /// snapshot time through the provider's `has_tool`, in `tool.rs`.
    pub(crate) fn base_tool_names_with(
        &self,
        nats_declarations: &[ToolDeclaration],
        provider_declarations: &[ToolDeclaration],
    ) -> HashSet<String> {
        self.tools
            .declarations()
            .iter()
            .map(|declaration| declaration.name.clone())
            .chain(
                nats_declarations
                    .iter()
                    .map(|declaration| declaration.name.clone()),
            )
            .chain(
                provider_declarations
                    .iter()
                    .map(|declaration| declaration.name.clone()),
            )
            .chain(BUILTIN_TOOLS.iter().map(|(name, _)| name.to_string()))
            .collect()
    }
}

/// A `ToolDeclaration` for one script tool. Hints match the `bash`
/// declaration's policy: a script tool wraps side-effecting inner calls, so
/// it is neither idempotent nor read-only.
// qual:allow(dry, boilerplate) reason: "declaration snapshots the shared definition; ToolDeclaration owns its fields, so the copies are the point of the snapshot"
pub(crate) fn script_tool_declaration(definition: &ScriptToolDefinition) -> ToolDeclaration {
    ToolDeclaration {
        name: definition.name.clone(),
        description: definition.description.clone(),
        parameters: definition.parameters.clone(),
        mcp_tool_name: None,
        mcp_server_name: None,
        call_template: None,
        result_template: None,
        idempotent_hint: Some(false),
        read_only_hint: Some(false),
        kind: None,
    }
}

/// Warn once per name per process: the accepted view is recomputed every
/// round, so without deduplication a colliding tool would warn on each one.
/// This is log bookkeeping only, never agent or dispatch state.
pub(crate) fn warn_collision(tool: &ResolvedScriptTool) {
    static WARNED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let name = tool.definition().name.clone();
    let mut warned = WARNED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if warned.get_or_insert_with(HashSet::new).insert(name.clone()) {
        log::warn!(
            "script tool '{name}' at {} collides with an existing tool of the same name; \
             the original tool stays callable and the script tool is disabled",
            tool.location()
        );
    }
}
