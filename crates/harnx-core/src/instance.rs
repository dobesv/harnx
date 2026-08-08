use std::fmt;
use uuid::Uuid;

/// Environment variable naming the scope a server registers under.
///
/// Assigned by whoever provisions a set of servers — a worker for its own
/// children, or the operator for an independently deployed set — and shared by
/// every member of that set. It namespaces every NATS subject and registry key,
/// so worker and servers must carry the same value.
pub const HARNX_SERVER_SCOPE: &str = "HARNX_SERVER_SCOPE";

/// Conventional scope for independently deployed servers shared by all sessions.
pub const SHARED_SCOPE: &str = "shared";

/// Identifier shared by a set of servers and the worker that talks to them.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ServerScope(String);

impl ServerScope {
    /// Create a new ID from this process's PID and a random UUID nonce.
    pub fn new() -> Self {
        Self(format!("{}-{}", std::process::id(), Uuid::new_v4()))
    }

    /// Reconstruct an ID received through the worker environment.
    pub fn from_string(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Build the Core NATS subject for a tool invocation.
    pub fn tool_subject(&self, identity_token: &str, tool: &str) -> String {
        format!("harnx.v1.{self}.tools.{identity_token}.{tool}")
    }

    /// Build the per-instance control subject used for cancel and progress.
    pub fn control_subject(&self) -> String {
        format!("harnx.v1.{self}.tools.control")
    }

    /// Build the Core NATS subject for a hook event.
    pub fn hook_subject(&self, server: &str, event: &str) -> String {
        format!("harnx.v1.{self}.hook.{server}.{event}")
    }
}

/// How a server binary can be run without a worker supplying its scope.
///
/// The three ways to run standalone are fixed, so a free-form hint would let a
/// caller pass text matching no real flag — an enum keeps the hint honest, and
/// keeps this file under CodeScene's string-argument threshold.
pub enum StandaloneMode {
    /// Toolset servers: serve MCP over stdio instead of registering over NATS.
    McpStdio,
    /// harnx-mcp-bridge: report the wrapped server's tools and exit.
    ListTools,
    /// Hook servers: no standalone mode; the worker launches them from config.
    WorkerLaunched,
}

impl StandaloneMode {
    fn hint(&self) -> &'static str {
        match self {
            Self::McpStdio => "--mcp-stdio",
            Self::ListTools => "--list-tools",
            Self::WorkerLaunched => "a hooks entry in your config, which the worker launches",
        }
    }
}

/// Explain a missing scope in terms of how the binary is meant to be launched.
///
/// The bare "variable is required" form sent operators looking for a value to
/// invent, when the real answer is either "let the worker launch this" or "use
/// the standalone stdio mode".
pub fn missing_scope_message(mode: StandaloneMode) -> String {
    let standalone_hint = mode.hint();
    format!(
        "{HARNX_SERVER_SCOPE} is not set.\n\
         This binary normally runs as a child of harnx-worker, which supplies it.\n\
         To run it standalone, use {standalone_hint}.\n\
         Do not set {HARNX_SERVER_SCOPE} by hand: it namespaces every NATS \
         subject and registry key, so a value that does not match the worker's \
         leaves this server undiscoverable."
    )
}

impl Default for ServerScope {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ServerScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_ids_are_unique_and_include_process_id() {
        let first = ServerScope::new();
        let second = ServerScope::new();

        assert_ne!(first, second);
        let pid_prefix = format!("{}-", std::process::id());
        assert!(first.as_str().starts_with(&pid_prefix));
        assert!(second.as_str().starts_with(&pid_prefix));
    }

    #[test]
    fn instance_subjects_are_wire_versioned_and_instance_scoped() {
        let instance_id = ServerScope::new();

        assert_eq!(
            instance_id.tool_subject("time", "now"),
            format!("harnx.v1.{instance_id}.tools.time.now")
        );
        assert_eq!(
            instance_id.control_subject(),
            format!("harnx.v1.{instance_id}.tools.control")
        );
        assert_eq!(
            instance_id.hook_subject("proxy-auth", "PreToolUse"),
            format!("harnx.v1.{instance_id}.hook.proxy-auth.PreToolUse")
        );
    }
}
