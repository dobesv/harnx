//! Package-aware tool selection and agent-owned whitelist merging.
use super::{tool_name_selector, AgentConfig, Config, ToolDeclaration};
use std::collections::HashSet;

impl Config {
    pub fn select_tools(&self, agent: &AgentConfig) -> Option<Vec<ToolDeclaration>> {
        let use_tools = agent.use_tools()?;
        let package = harnx_core::package_namespace::pkg_from_qualified(agent.name());
        let functions = self.select_tools_for_package(&use_tools, package);
        (!functions.is_empty()).then_some(functions)
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
