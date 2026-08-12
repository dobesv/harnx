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

    /// Reconstruct an ID from `HARNX_SERVER_SCOPE`. Usually set by a worker
    /// for the children it launches, but an operator sets it directly too
    /// when deploying tool/hook servers independently of any worker (see
    /// `docs/nats-ha.md`).
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
    /// A full clause describing what to do about the missing scope, phrased
    /// so it reads correctly whether or not a true standalone mode exists —
    /// `WorkerLaunched` has none, so its own text says so instead of the
    /// generic "run standalone with X" wrapper the other two variants use.
    fn hint(&self) -> &'static str {
        match self {
            Self::McpStdio => "run it standalone with --mcp-stdio",
            Self::ListTools => "run it standalone with --list-tools",
            Self::WorkerLaunched => {
                "configure a hooks entry in your config; there is no standalone \
                 mode for hook servers, the worker always launches them"
            }
        }
    }
}

/// Explain a missing scope in terms of how the binary is meant to be launched.
///
/// The bare "variable is required" form sent operators looking for a value to
/// invent, when the real answer is either "let the worker launch this" or "use
/// the standalone stdio mode".
pub fn missing_scope_message(mode: StandaloneMode) -> String {
    let hint = mode.hint();
    format!(
        "{HARNX_SERVER_SCOPE} is not set to a non-empty value.\n\
         This binary normally runs as a child of harnx-worker, which supplies it.\n\
         To proceed, {hint}.\n\
         If you are instead deploying this server independently of any worker \
         (see docs/nats-ha.md), set {HARNX_SERVER_SCOPE} yourself to a value \
         every worker and server in the set shares exactly — it namespaces \
         every NATS subject and registry key, so a mismatched value leaves \
         this server undiscoverable."
    )
}

/// Read [`HARNX_SERVER_SCOPE`] from the environment and build a
/// [`ServerScope`] from it, treating an empty value the same as an absent one.
///
/// `std::env::var` returns `Ok("")` when the variable is set but empty.
/// `ServerScope::from_string("")` would happily build a scope that strips no
/// prefix from any subject or registration key: the server starts and
/// registers normally, and no worker configured for a real scope can ever
/// find it. There is no way to fail closed here other than rejecting the
/// value up front, the same as if it had never been set.
pub fn scope_from_env(mode: StandaloneMode) -> anyhow::Result<ServerScope> {
    std::env::var(HARNX_SERVER_SCOPE)
        .ok()
        .filter(|value| !value.is_empty())
        .map(ServerScope::from_string)
        .ok_or_else(|| anyhow::anyhow!(missing_scope_message(mode)))
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
