/// Sanitize a string for use in tool name generation.
/// Replaces `/` with `__` and any other non-alphanumeric-or-hyphen char with `_`.
/// Examples:
///   "my-pkg"      → "my-pkg"
///   "org/my-pkg"  → "org__my-pkg"
///   "foo bar"     → "foo_bar"
pub fn sanitize_for_tool_name(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        if ch == '/' {
            result.push_str("__");
        } else if ch.is_alphanumeric() || ch == '-' || ch == '_' {
            result.push(ch);
        } else {
            result.push('_');
        }
    }
    result
}

/// Given a package name and bare agent name (stem from .md filename),
/// return the qualified agent name as seen outside the package.
/// Example: ("mypkg", "coder") → "mypkg/coder"
pub fn qualify_agent_name(pkg: &str, agent_stem: &str) -> String {
    format!("{pkg}/{agent_stem}")
}

/// Given a qualified agent name like "mypkg/coder", return tool name prefix.
/// "mypkg/coder" → "mypkg__coder"
/// Used so ACP tool names become "mypkg__coder_session_prompt".
pub fn agent_tool_prefix(qualified_agent_name: &str) -> String {
    sanitize_for_tool_name(qualified_agent_name)
}

/// Given a package name and MCP server name, return cross-package tool prefix.
/// ("mypkg", "fs") → "mypkg__fs"
/// So tool "read_file" becomes "mypkg__fs_read_file" from outside package.
pub fn mcp_tool_prefix(pkg: &str, server_name: &str) -> String {
    format!("{}__{server_name}", sanitize_for_tool_name(pkg))
}

/// Given a qualified agent name ("mypkg/coder"), extract package name.
pub fn pkg_from_qualified(qualified: &str) -> Option<&str> {
    qualified.split_once('/').map(|(pkg, _)| pkg)
}

/// Given a package name and bare use_tools entry from agent's frontmatter,
/// rewrite it to namespaced form unless it already contains `__`
/// (which signals an explicit already-qualified reference).
/// Examples:
///   pkg="mypkg", entry="fs_read_file"        → "mypkg__fs_read_file"
///   pkg="mypkg", entry="mypkg__fs_read_file" → "mypkg__fs_read_file" (unchanged)
///   pkg="mypkg", entry="fs_{read,write}"     → "mypkg__fs_{read,write}"
pub fn namespace_use_tools_entry(pkg: &str, entry: &str) -> String {
    if entry.contains("__") {
        // Already explicitly qualified — leave as-is
        entry.to_string()
    } else {
        format!("{}__{entry}", sanitize_for_tool_name(pkg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_slash() {
        assert_eq!(sanitize_for_tool_name("mypkg/coder"), "mypkg__coder");
    }

    #[test]
    fn test_sanitize_clean() {
        assert_eq!(sanitize_for_tool_name("mypkg"), "mypkg");
    }

    #[test]
    fn test_qualify_agent() {
        assert_eq!(qualify_agent_name("mypkg", "coder"), "mypkg/coder");
    }

    #[test]
    fn test_agent_tool_prefix() {
        assert_eq!(agent_tool_prefix("mypkg/coder"), "mypkg__coder");
    }

    #[test]
    fn test_mcp_tool_prefix() {
        assert_eq!(mcp_tool_prefix("mypkg", "fs"), "mypkg__fs");
    }

    #[test]
    fn test_pkg_from_qualified() {
        assert_eq!(pkg_from_qualified("mypkg/coder"), Some("mypkg"));
        assert_eq!(pkg_from_qualified("bare"), None);
    }

    #[test]
    fn test_namespace_use_tools_plain() {
        assert_eq!(
            namespace_use_tools_entry("mypkg", "fs_read_file"),
            "mypkg__fs_read_file"
        );
    }

    #[test]
    fn test_namespace_use_tools_already_qualified() {
        assert_eq!(
            namespace_use_tools_entry("mypkg", "mypkg__fs_read_file"),
            "mypkg__fs_read_file"
        );
    }

    #[test]
    fn test_namespace_use_tools_glob() {
        assert_eq!(
            namespace_use_tools_entry("mypkg", "fs_{read_file,write_file}"),
            "mypkg__fs_{read_file,write_file}"
        );
    }
}
