mod client;
mod config;
mod convert;

#[allow(unused_imports)]
pub use client::McpManager;
#[allow(unused_imports)]
pub use config::McpServerConfig;

#[allow(unused_imports)]
use convert::mcp_tool_to_function;

pub fn is_mcp_tool(name: &str) -> bool {
    name.starts_with("mcp__")
}

pub fn extract_server_name(tool_name: &str) -> Option<String> {
    let rest = tool_name.strip_prefix("mcp__")?;
    let (server_name, _) = rest.split_once("__")?;
    if server_name.is_empty() {
        None
    } else {
        Some(server_name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{extract_server_name, is_mcp_tool};

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

    #[test]
    fn mcp_extract_server_name_parses_prefixed_tool_names() {
        assert_eq!(
            extract_server_name("mcp__filesystem__read_file"),
            Some("filesystem".to_string())
        );
        assert_eq!(
            extract_server_name("mcp__git__log"),
            Some("git".to_string())
        );
        assert_eq!(
            extract_server_name("mcp__my_server__tool_name"),
            Some("my_server".to_string())
        );
    }

    #[test]
    fn mcp_extract_server_name_rejects_invalid_names() {
        assert_eq!(extract_server_name("read_file"), None);
        assert_eq!(extract_server_name("mcp__"), None);
        assert_eq!(extract_server_name("mcp____tool"), None);
        assert_eq!(extract_server_name("mcp__filesystem"), None);
    }
}
