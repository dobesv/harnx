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
    name.starts_with("mcp__")
}

#[cfg(test)]
mod tests {
    use super::is_mcp_tool;

    #[test]
    fn mcp_is_mcp_tool_matches_prefixed_names() {
        assert!(is_mcp_tool("mcp__filesystem__read_file"));
        assert!(is_mcp_tool("mcp__git__log"));
        assert!(is_mcp_tool("mcp__my_server__tool_name"));
    }

    #[test]
    fn mcp_is_mcp_tool_rejects_non_prefixed_names() {
        assert!(!is_mcp_tool("read_file"));
        assert!(!is_mcp_tool("tool__mcp__filesystem"));
        assert!(!is_mcp_tool(""));
    }
}
