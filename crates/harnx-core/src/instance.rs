use std::fmt;
use uuid::Uuid;

/// Environment variable used to pass a worker's instance ID to tool servers.
pub const HARNX_INSTANCE_ID: &str = "HARNX_INSTANCE_ID";

/// Process-lifetime identifier shared by a worker and all of its tool servers.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct InstanceId(String);

impl InstanceId {
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
    pub fn tool_subject(&self, server: &str, tool: &str) -> String {
        format!("harnx.v1.{self}.tools.{server}.{tool}")
    }

    /// Build the per-instance control subject used for cancel and progress.
    pub fn control_subject(&self) -> String {
        format!("harnx.v1.{self}.tools.control")
    }
}

impl Default for InstanceId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_ids_are_unique_and_include_process_id() {
        let first = InstanceId::new();
        let second = InstanceId::new();

        assert_ne!(first, second);
        let pid_prefix = format!("{}-", std::process::id());
        assert!(first.as_str().starts_with(&pid_prefix));
        assert!(second.as_str().starts_with(&pid_prefix));
    }

    #[test]
    fn instance_subjects_are_wire_versioned_and_instance_scoped() {
        let instance_id = InstanceId::new();

        assert_eq!(
            instance_id.tool_subject("time", "now"),
            format!("harnx.v1.{instance_id}.tools.time.now")
        );
        assert_eq!(
            instance_id.control_subject(),
            format!("harnx.v1.{instance_id}.tools.control")
        );
    }
}
