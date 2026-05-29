//! Helpers for inspecting the connected MCP peer (client) from a server.

use rmcp::service::Peer;
use rmcp::RoleServer;

/// Returns `true` when the connected client advertised support for the
/// `roots` capability during initialization.
///
/// MCP servers must only send `roots/list` requests to clients that
/// declared the `roots` capability. Calling `list_roots` against a client
/// that never advertised it yields a protocol error, so callers use this
/// to decide whether to fall back to their CLI-provided roots instead.
pub fn peer_supports_roots(peer: &Peer<RoleServer>) -> bool {
    peer.peer_info()
        .map(|info| info.capabilities.roots.is_some())
        .unwrap_or(false)
}
