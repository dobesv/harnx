mod client;
mod config;
mod convert;

#[allow(unused_imports)]
pub use client::McpManager;
#[allow(unused_imports)]
pub use config::McpServerConfig;

#[allow(unused_imports)]
use convert::mcp_tool_to_declaration;

pub fn is_mcp_tool(name: &str) -> bool {
    name.contains('_')
}

#[cfg(test)]
mod tests {
    use super::is_mcp_tool;

    #[test]
    fn mcp_is_mcp_tool_matches_prefixed_names() {
        assert!(is_mcp_tool("filesystem_read_file"));
        assert!(is_mcp_tool("git_log"));
        assert!(is_mcp_tool("my_server_tool_name"));
    }

    #[test]
    fn mcp_is_mcp_tool_rejects_names_without_underscores() {
        assert!(!is_mcp_tool("readfile"));
        assert!(!is_mcp_tool("toolname"));
        assert!(!is_mcp_tool(""));
    }
}
