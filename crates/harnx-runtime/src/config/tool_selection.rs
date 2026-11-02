//! Package-aware tool selection and agent-owned whitelist merging.
use super::{
    bash_declaration::{synthesize_bash_declaration, BASH_TOOL_NAME},
    tool_name_selector, AgentConfig, Config, ToolDeclaration,
};
use std::collections::HashSet;

impl Config {
    pub fn select_tools(&self, agent: &AgentConfig) -> Option<Vec<ToolDeclaration>> {
        let package = harnx_core::package_namespace::pkg_from_qualified(agent.name());
        let mut functions = agent
            .use_tools()
            .map(|use_tools| self.select_tools_for_package(&use_tools, package))
            .unwrap_or_default();
        let bash_inner = self.select_bash_inner_tools(
            self.effective_bash_tools(agent).as_deref().unwrap_or(&[]),
            package,
        );
        if !bash_inner.is_empty() {
            // While script mode is active the name `bash` is reserved: the
            // model gets the synthesized declaration, and an external tool of
            // the same name is not exposed. Dispatch goes with the reserved
            // stub provider (see `bash_provider`), so a `bash` call can never
            // route to a raw-alias external tool either.
            functions.retain(|v| v.name != BASH_TOOL_NAME);
            functions.push(synthesize_bash_declaration(&bash_inner));
        }
        (!functions.is_empty()).then_some(functions)
    }

    /// The `bash_tools` selectors tool selection resolves for `agent`.
    ///
    /// For the active agent, the active session's record wins over the
    /// explicit agent's: `.set bash_tools` stays scoped to the session
    /// instead of overwriting the agent (which the TUI session picker keeps
    /// across a switch, so a write there would leak session A's selectors
    /// into session B). The session copies the agent's value when it is
    /// created, so without an override both agree. Agents other than the
    /// active one (e.g. a compaction summarizer) keep their own
    /// `bash_tools`, so a session override never bleeds into their tool
    /// selection.
    fn effective_bash_tools(&self, agent: &AgentConfig) -> Option<Vec<String>> {
        if agent.name() == self.extract_agent().name() {
            if let Some(bash_tools) = self
                .session
                .as_ref()
                .and_then(|session| session.bash_tools())
            {
                return Some(bash_tools);
            }
        }
        agent.bash_tools()
    }

    /// Whether the active agent's `bash_tools` (the session's record over the
    /// agent's own, as resolved by `effective_bash_tools`) resolves to a
    /// non-empty inner set, meaning the `bash` tool name is reserved for
    /// script mode.
    pub fn bash_tools_active(&self) -> bool {
        let agent = self.extract_agent();
        !self.bash_tool_names(&agent).is_empty()
    }

    /// The inner tool set script mode exposes as shell commands, resolved from
    /// the agent's `bash_tools` selectors. Independent of `use_tools`: a tool
    /// can be bash-only. Empty when `bash_tools` is unset, empty, matches
    /// nothing, or tool use is disabled. Sorted by name for stable
    /// declaration descriptions and provider wiring.
    pub fn bash_tool_names(&self, agent: &AgentConfig) -> Vec<String> {
        let package = harnx_core::package_namespace::pkg_from_qualified(agent.name());
        self.select_bash_inner_tools(
            self.effective_bash_tools(agent).as_deref().unwrap_or(&[]),
            package,
        )
        .into_iter()
        .map(|declaration| declaration.name)
        .collect()
    }

    /// Resolve `bash_tools` selectors against the available declarations using
    /// the same machinery as `use_tools`, then drop names that must stay at
    /// the model boundary.
    fn select_bash_inner_tools(
        &self,
        bash_tools: &[String],
        package: Option<&str>,
    ) -> Vec<ToolDeclaration> {
        if !self.tool_use || bash_tools.is_empty() {
            return Vec::new();
        }
        let selectors_str = bash_tools.join(",");
        let (declarations, _) = self.tool_declarations_for_use_tools(Some(&selectors_str), package);
        let tool_names = self.collect_selected_tool_names(bash_tools, &declarations);
        let mut inner: Vec<ToolDeclaration> = declarations
            .into_iter()
            .filter(|declaration| {
                tool_names.contains(&declaration.name)
                    && is_bash_inner_tool_allowed(&declaration.name)
            })
            .collect();
        inner.sort_by(|a, b| a.name.cmp(&b.name));
        inner
    }

    /// Select tools without requiring an agent configuration.
    ///
    /// Cached tool declarations must already use the names visible from `package`
    /// (same-package bare names, cross-package qualified names). The package also
    /// controls the spelling of generated handoff declarations. Returns an empty
    /// list when tool use is disabled or no selectors match.
    pub fn select_tools_for_package(
        &self,
        use_tools: &[String],
        package: Option<&str>,
    ) -> Vec<ToolDeclaration> {
        if !self.tool_use {
            return Vec::new();
        }
        let use_tools_str = use_tools.join(",");
        let (declarations, _) = self.tool_declarations_for_use_tools(Some(&use_tools_str), package);
        let tool_names = self.collect_selected_tool_names(use_tools, &declarations);

        let mut functions: Vec<ToolDeclaration> = declarations
            .iter()
            .filter(|v| tool_names.contains(&v.name))
            .cloned()
            .collect();
        self.merge_agent_owned_tools(&mut functions, &tool_names);

        functions
    }

    /// Resolve the concrete set of tool names selected by `use_tools`,
    /// expanding toolset names and glob selectors against the available
    /// `declarations`.
    fn collect_selected_tool_names(
        &self,
        use_tools: &[String],
        declarations: &[ToolDeclaration],
    ) -> HashSet<String> {
        let declaration_names: HashSet<String> =
            declarations.iter().map(|v| v.name.to_string()).collect();
        let mut tool_names: HashSet<String> = HashSet::new();
        for item in use_tools.iter().map(|s| s.trim()) {
            let sanitized_item = harnx_core::package_namespace::sanitize_for_tool_name(item);
            if let Some(values) = self.toolsets.get(item).or_else(|| {
                (sanitized_item != item)
                    .then(|| self.toolsets.get(&sanitized_item))
                    .flatten()
            }) {
                tool_names.extend(
                    values
                        .iter()
                        .filter(|v| declaration_names.contains(v.as_str()))
                        .cloned(),
                );
            } else {
                let selector = tool_name_selector(item);
                let sanitized_selector =
                    (sanitized_item != item).then(|| tool_name_selector(&sanitized_item));
                tool_names.extend(
                    declaration_names
                        .iter()
                        .filter(|name| {
                            selector.is_match(name)
                                || sanitized_selector
                                    .as_ref()
                                    .is_some_and(|sanitized| sanitized.is_match(name))
                        })
                        .cloned(),
                );
            }
        }
        tool_names
    }

    /// Merge in any agent-owned tool declarations (e.g. handoff tools, builtins)
    /// that are permitted by `tool_names` but not already present in `functions`.
    /// The `tool_names` whitelist ensures `agent.tools()` cannot smuggle in tools
    /// that `use_tools` did not request.
    fn merge_agent_owned_tools(
        &self,
        functions: &mut Vec<ToolDeclaration>,
        tool_names: &HashSet<String>,
    ) {
        let Some(active_agent) = &self.agent else {
            return;
        };
        let existing_names: HashSet<String> =
            functions.iter().map(|v| v.name.to_string()).collect();
        functions.extend(
            active_agent
                .tools()
                .declarations()
                .into_iter()
                .filter(|v| tool_names.contains(&v.name) && !existing_names.contains(&v.name)),
        );
    }
}

/// Whether a tool name may appear in the script-mode inner set. The script
/// tool itself and the control-flow tools must stay at the model boundary:
/// a script cannot spawn another script, hand the session to another agent,
/// or park on a human-in-the-loop prompt.
pub(crate) fn is_bash_inner_tool_allowed(name: &str) -> bool {
    name != BASH_TOOL_NAME
        && name != "switch_agent"
        && name != "ask_user"
        && !name.ends_with("_session_handoff")
}
