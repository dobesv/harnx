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

/// Compute package-aware handoff display name for a target agent.
///
/// The returned name is always sanitized to the tool-name charset
/// (`[A-Za-z0-9_-]`, see [`sanitize_for_tool_name`]) so it is valid for LLM
/// provider function-name schemas regardless of what characters the source
/// agent filename/package contains.
///
/// Rules:
/// - same-package peer from within a package → bare stem (`atlas`)
/// - cross-package peer → namespaced form (`otherpkg__atlas`)
/// - top-level peer from within a package → explicit top-level (`__reviewer`)
/// - top-level context → bare top-level peers, namespaced package peers
pub fn handoff_display_name(target_agent_name: &str, active_pkg: Option<&str>) -> String {
    match (pkg_from_qualified(target_agent_name), active_pkg) {
        (Some(target_pkg), Some(active_pkg)) if target_pkg == active_pkg => target_agent_name
            .split_once('/')
            .map(|(_, stem)| sanitize_for_tool_name(stem))
            .unwrap_or_else(|| sanitize_for_tool_name(target_agent_name)),
        (Some(_), _) => agent_tool_prefix(target_agent_name),
        (None, Some(_)) => format!("__{}", sanitize_for_tool_name(target_agent_name)),
        (None, None) => sanitize_for_tool_name(target_agent_name),
    }
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

/// Resolves a potentially bare agent/client/model name relative to a package context.
///
/// Rules:
///   - `/foo`        → `"foo"` (explicit top-level, strip leading slash)
///   - `other/foo`   → `"other/foo"` (already qualified, unchanged)
///   - `foo` + Some("pkg") → `"pkg/foo"` (relative to current package)
///   - `foo` + None  → `"foo"` (top-level context, unchanged)
pub fn resolve_package_relative_name(name: &str, pkg_context: Option<&str>) -> String {
    if let Some(stripped) = name.strip_prefix('/') {
        // Leading slash = explicit top-level escape
        stripped.to_string()
    } else if name.contains('/') {
        // Already qualified (cross-package or explicit)
        name.to_string()
    } else if let Some(pkg) = pkg_context {
        // Bare name in a package context → qualify it
        format!("{pkg}/{name}")
    } else {
        // Bare name at top level → unchanged
        name.to_string()
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
    fn test_handoff_display_name_same_package_peer() {
        assert_eq!(
            handoff_display_name("pantheon/atlas", Some("pantheon")),
            "atlas"
        );
    }

    #[test]
    fn test_handoff_display_name_cross_package_peer() {
        assert_eq!(
            handoff_display_name("otherpkg/helper", Some("pantheon")),
            "otherpkg__helper"
        );
    }

    #[test]
    fn test_handoff_display_name_top_level_from_package() {
        assert_eq!(
            handoff_display_name("reviewer", Some("pantheon")),
            "__reviewer"
        );
    }

    #[test]
    fn test_handoff_display_name_top_level_context() {
        assert_eq!(handoff_display_name("reviewer", None), "reviewer");
        assert_eq!(
            handoff_display_name("pantheon/atlas", None),
            "pantheon__atlas"
        );
    }

    #[test]
    fn test_handoff_display_name_sanitizes_invalid_chars() {
        // Provider-invalid characters (e.g. spaces) in agent filenames must
        // never leak into the tool name, for every branch.
        assert_eq!(handoff_display_name("my agent", None), "my_agent");
        assert_eq!(
            handoff_display_name("my agent", Some("pantheon")),
            "__my_agent"
        );
        assert_eq!(
            handoff_display_name("pantheon/my agent", Some("pantheon")),
            "my_agent"
        );
        // Resulting names are always within the tool-name charset.
        for name in [
            handoff_display_name("a b", None),
            handoff_display_name("a b", Some("p")),
            handoff_display_name("p/a b", Some("p")),
            handoff_display_name("other/a b", Some("p")),
        ] {
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "unsanitized display name: {name}"
            );
        }
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

    #[test]
    fn test_resolve_package_relative_bare_with_context() {
        assert_eq!(
            resolve_package_relative_name("foo", Some("mypkg")),
            "mypkg/foo"
        );
    }

    #[test]
    fn test_resolve_package_relative_bare_no_context() {
        assert_eq!(resolve_package_relative_name("foo", None), "foo");
    }

    #[test]
    fn test_resolve_package_relative_leading_slash_with_context() {
        assert_eq!(resolve_package_relative_name("/foo", Some("mypkg")), "foo");
    }

    #[test]
    fn test_resolve_package_relative_leading_slash_no_context() {
        assert_eq!(resolve_package_relative_name("/foo", None), "foo");
    }

    #[test]
    fn test_resolve_package_relative_qualified_with_context() {
        assert_eq!(
            resolve_package_relative_name("other/foo", Some("mypkg")),
            "other/foo"
        );
    }

    #[test]
    fn test_resolve_package_relative_qualified_no_context() {
        assert_eq!(
            resolve_package_relative_name("other/foo", None),
            "other/foo"
        );
    }
}
