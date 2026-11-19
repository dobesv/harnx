use async_trait::async_trait;
use harnx_toolset::{ToolInvokeError, ToolProgressKind, ToolSpec, Toolset};
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

/// The bearer authorizes the local credentials endpoint; AWS keys stay out of env.
pub(super) fn environment(port: u16, token: &str, region: &str) -> Map<String, Value> {
    Map::from_iter([
        (
            "AWS_CONTAINER_CREDENTIALS_FULL_URI".into(),
            json!(format!("http://127.0.0.1:{port}/creds")),
        ),
        ("AWS_CONTAINER_AUTHORIZATION_TOKEN".into(), json!(token)),
        ("AWS_REGION".into(), json!(region)),
    ])
}

pub(super) struct AwsCredentialsToolset {
    reply: Value,
}

impl AwsCredentialsToolset {
    pub fn new(port: u16, token: &str, region: &str) -> Self {
        Self {
            // AWS uses the container-credentials protocol and provisions no files.
            reply: json!({"env": environment(port, token, region), "filesystem": {}}),
        }
    }
}

#[async_trait]
impl Toolset for AwsCredentialsToolset {
    fn name(&self) -> &str {
        "aws_creds"
    }

    fn default_mcp_http_port(&self) -> u16 {
        3010
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "environment".into(),
            description: "Return AWS container-credential environment additions. Replace matching caller env keys. The local credential service must stay running beside bash tools.".into(),
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
                "AWS environment call cancelled".into(),
            ));
        }
        if tool != "environment" {
            return Err(ToolInvokeError::Recoverable(format!(
                "unknown AWS credentials tool: {tool}"
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
    async fn stable_environment_has_no_raw_aws_keys_or_files() {
        let toolset = AwsCredentialsToolset::new(1234, "capability", "us-west-2");
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
            json!({"env": {
            "AWS_CONTAINER_CREDENTIALS_FULL_URI":"http://127.0.0.1:1234/creds",
            "AWS_CONTAINER_AUTHORIZATION_TOKEN":"capability", "AWS_REGION":"us-west-2",
        }, "filesystem":{}})
        );
        let spec = toolset.tools().remove(0);
        assert!(spec.idempotent_hint && spec.read_only_hint);
    }

    #[tokio::test]
    async fn rejects_arguments_unknown_tool_and_cancelled_calls() {
        let toolset = AwsCredentialsToolset::new(1, "token", "region");
        for (tool, args) in [
            ("other", json!({})),
            ("environment", Value::Null),
            ("environment", json!({"profile":"other"})),
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
