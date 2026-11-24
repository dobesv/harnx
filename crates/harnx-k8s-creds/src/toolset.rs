use async_trait::async_trait;
use harnx_toolset::{ToolInvokeError, ToolProgressKind, ToolSpec, Toolset};
use serde_json::{json, Map, Value};
use std::path::Path;
use tokio_util::sync::CancellationToken;

/// Build the environment map returned by the tool and merged into the sandbox.
pub(super) fn environment(kubeconfig_path: &Path) -> Map<String, Value> {
    Map::from_iter([(
        "KUBECONFIG".into(),
        json!(kubeconfig_path.to_string_lossy()),
    )])
}

#[derive(Clone)]
pub(super) struct K8sCredsToolset {
    reply: Value,
}

impl K8sCredsToolset {
    pub fn new(kubeconfig_path: &Path) -> Self {
        Self {
            reply: json!({
                "env": environment(kubeconfig_path),
                "filesystem": {
                    "kubeconfig_path": kubeconfig_path.to_string_lossy(),
                },
            }),
        }
    }
}

#[async_trait]
impl Toolset for K8sCredsToolset {
    fn name(&self) -> &str {
        "k8s_creds"
    }

    fn default_mcp_http_port(&self) -> u16 {
        3011
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "environment".into(),
            description: "Return Kubernetes environment defaults and already-provisioned kubeconfig location on the bash host. Replace matching caller env keys. The local credential service must stay running beside bash tools.".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            cancellation_guarantee: harnx_toolset::CancellationGuarantee::Cooperative,
            read_only_hint: true,
            idempotent_hint: true,
            timeout_secs: None,
            meta: None,
        }.with_kind(ToolProgressKind::Read)]
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        if cancel.is_cancelled() {
            return Err(ToolInvokeError::Fatal(
                "k8s creds environment call cancelled".into(),
            ));
        }
        if tool != "environment" {
            return Err(ToolInvokeError::Recoverable(format!(
                "unknown k8s creds tool: {tool}"
            )));
        }
        if !args.as_object().is_some_and(Map::is_empty) {
            return Err(ToolInvokeError::Recoverable(
                "environment expects an empty object".into(),
            ));
        }
        Ok(self.reply.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stable_environment_has_kubeconfig_and_filesystem_paths() {
        let toolset = K8sCredsToolset::new(Path::new("/synthetic/kubeconfig"));
        let first = toolset
            .invoke("environment", json!({}), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            first,
            toolset
                .invoke("environment", json!({}), CancellationToken::new())
                .await
                .unwrap()
        );
        assert_eq!(
            first,
            json!({
                "env": {
                    "KUBECONFIG": "/synthetic/kubeconfig",
                },
                "filesystem": {
                    "kubeconfig_path": "/synthetic/kubeconfig",
                },
            })
        );
        let spec = toolset.tools().remove(0);
        assert!(spec.idempotent_hint && spec.read_only_hint);
        assert_eq!(spec.kind(), Some(ToolProgressKind::Read));
    }

    #[tokio::test]
    async fn rejects_arguments_unknown_tool_and_cancelled_calls() {
        let toolset = K8sCredsToolset::new(Path::new("/synthetic/kubeconfig"));
        for (tool, args) in [
            ("other", json!({})),
            ("environment", Value::Null),
            ("environment", json!({"extra": "argument"})),
        ] {
            assert!(matches!(
                toolset.invoke(tool, args, CancellationToken::new()).await,
                Err(ToolInvokeError::Recoverable(_))
            ));
        }
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            toolset.invoke("environment", json!({}), cancel).await,
            Err(ToolInvokeError::Fatal(_))
        ));
    }

    #[test]
    fn env_precedence_parity_overwrites_existing_kubeconfig() {
        let path = Path::new("/synthetic/kubeconfig");
        let additions = environment(path);
        let mut caller_env = Map::from_iter([
            ("KUBECONFIG".to_string(), json!("/old/caller/kubeconfig")),
            ("OTHER_KEY".to_string(), json!("other_value")),
        ]);
        caller_env.extend(additions);
        assert_eq!(caller_env["KUBECONFIG"], "/synthetic/kubeconfig");
        assert_eq!(caller_env["OTHER_KEY"], "other_value");
    }

    #[tokio::test]
    async fn toolset_honors_enable_tool_filter() {
        let toolset = K8sCredsToolset::new(Path::new("/synthetic/kubeconfig"));
        let mut builder = globset::GlobSetBuilder::new();
        builder.add(globset::Glob::new("no-match").unwrap());
        let filter = builder.build().unwrap();
        let filtered = harnx_toolset_server::FilteredToolset::new(toolset, filter);
        assert!(filtered.tools().is_empty());
    }
}
