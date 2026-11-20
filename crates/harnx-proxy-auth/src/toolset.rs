//! Ordinary tool API. Provisioning and the proxy stay alive in the hosting process.
use async_trait::async_trait;
use harnx_toolset::{ToolInvokeError, ToolProgressKind, ToolSpec, Toolset};
use serde_json::{json, Map, Value};
use std::path::Path;
use tokio_util::sync::CancellationToken;

/// Snapshot of server-owned environment defaults and already-provisioned paths.
///
/// The host must keep the proxy and its temporary directories alive while serving
/// this toolset. Paths and the loopback proxy address refer to the bash tool host,
/// not the worker running middleware.
pub struct ProxyAuthToolset {
    environment: Value,
}

impl ProxyAuthToolset {
    pub fn new(
        proxy_port: u16,
        ca_cert_path: &Path,
        extra_env: &Map<String, Value>,
        temp_file_root: Option<&Path>,
    ) -> Self {
        Self {
            environment: json!({
                "env": crate::environment::defaults(proxy_port, &ca_cert_path.to_string_lossy(), extra_env),
                "filesystem": {
                    "ca_cert_path": ca_cert_path,
                    "temp_file_root": temp_file_root,
                },
            }),
        }
    }
}

#[async_trait]
impl Toolset for ProxyAuthToolset {
    fn name(&self) -> &str {
        "proxy_auth"
    }

    fn default_mcp_http_port(&self) -> u16 {
        3009
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "environment".into(),
            description: "Return proxy environment defaults and already-provisioned filesystem locations on the bash host. Merge env without replacing caller keys. The proxy service must stay running.".into(),
            input_schema: json!({"type":"object", "properties":{}, "additionalProperties":false}),
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
                "proxy environment call cancelled".into(),
            ));
        }
        if tool != "environment" {
            return Err(ToolInvokeError::Recoverable(format!(
                "unknown proxy auth tool: {tool}"
            )));
        }
        if !args.as_object().is_some_and(Map::is_empty) {
            return Err(ToolInvokeError::Recoverable(
                "environment expects an empty object".into(),
            ));
        }
        Ok(self.environment.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn environment_is_stable_and_has_no_legacy_hook_envelope() {
        let extra = Map::from_iter([
            ("ACLI_CONFIG_DIR".into(), json!("/tmp/acli")),
            ("HTTP_PROXY".into(), json!("http://override")),
        ]);
        let toolset = ProxyAuthToolset::new(
            4312,
            Path::new("/tmp/ca.pem"),
            &extra,
            Some(Path::new("/tmp/root")),
        );
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
        assert_eq!(first["env"]["ACLI_CONFIG_DIR"], "/tmp/acli");
        assert_eq!(first["env"]["HTTP_PROXY"], "http://override");
        assert_eq!(first["env"]["HTTPS_PROXY"], "http://127.0.0.1:4312");
        for key in [
            "SSL_CERT_FILE",
            "CURL_CA_BUNDLE",
            "NODE_EXTRA_CA_CERTS",
            "REQUESTS_CA_BUNDLE",
            "GIT_SSL_CAINFO",
        ] {
            assert_eq!(first["env"][key], "/tmp/ca.pem");
        }
        assert_eq!(first["filesystem"]["temp_file_root"], "/tmp/root");
        assert_eq!(first["filesystem"]["ca_cert_path"], "/tmp/ca.pem");
        assert!(first.get("hookSpecificOutput").is_none());
        let spec = toolset.tools().remove(0);
        assert!(spec.read_only_hint && spec.idempotent_hint);
        assert_eq!(spec.kind(), Some(ToolProgressKind::Read));
    }

    #[tokio::test]
    async fn rejects_unknown_tools_arguments_and_cancelled_calls() {
        let toolset = ProxyAuthToolset::new(1, Path::new("ca.pem"), &Map::new(), None);
        for (tool, args) in [
            ("unknown", json!({})),
            ("environment", json!({"command":"no"})),
            ("environment", Value::Null),
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
}
