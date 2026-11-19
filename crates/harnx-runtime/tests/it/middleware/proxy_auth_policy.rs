//! Platform-independent coverage of shipped policy, including helper failures.
use super::fixture::Fixture;
use harnx_core::{
    abort::{create_abort_signal, AbortSignal},
    middleware::MiddlewareOrigin,
    tool::{ToolCall, ToolError, ToolProvider, ToolProviderOutput},
};
use harnx_engine::tool::eval_tool_calls;
use harnx_runtime::config::{Config, ConfigLock, ToolServerConfig};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};

struct EnvironmentProvider {
    reply: Value,
    calls: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ToolProvider for EnvironmentProvider {
    fn name(&self) -> &str {
        "proxy-policy-fixture"
    }
    fn has_tool(&self, name: &str) -> bool {
        matches!(name, "proxy_auth_environment" | "bash_exec" | "bash_spawn")
    }
    async fn call_tool(
        &self,
        name: &str,
        args: Value,
        _abort: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        self.calls.lock().push(name.into());
        Ok(ToolProviderOutput::new(
            if name == "proxy_auth_environment" {
                self.reply.clone()
            } else {
                args
            },
        ))
    }
}

async fn run(package: &str, tool: &str, reply: Value, env: Value) -> (Value, Vec<String>) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages")
        .join(package);
    let path = root.join("tool_servers/bash.yaml");
    let mut server: ToolServerConfig =
        serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    server.name = "bash".into();
    server.middleware.origin = Some(MiddlewareOrigin {
        file: path,
        base_dir: root,
    });
    let config = Config {
        tool_servers: vec![server],
        ..Default::default()
    };
    *config.nats_tool_declarations.write() = ["proxy_auth_environment", "bash_exec", "bash_spawn"]
        .into_iter()
        .map(|name| {
            serde_json::from_value(
                json!({"name":name,"description":"fixture","parameters":{"type":"object"}}),
            )
            .unwrap()
        })
        .collect();
    let config = Arc::new(ConfigLock::new(config));
    let mut fixture = Fixture::with_config(config).await;
    let provider = Arc::new(EnvironmentProvider {
        reply,
        calls: Mutex::new(vec![]),
    });
    fixture.ctx.providers.push(provider.clone());
    let result = Box::pin(eval_tool_calls(
        &fixture.ctx,
        vec![ToolCall::new(
            tool.into(),
            json!({"command":"unchanged","env":env}),
            Some("outer".into()),
            None,
        )],
        &create_abort_signal(),
    ))
    .await
    .unwrap()
    .remove(0);
    let calls = provider.calls.lock().clone();
    (result.output, calls)
}

#[tokio::test]
async fn middleware_proxy_policy_merges_shipped_script_and_preserves_caller_env() {
    for package in ["coding", "pantheon"] {
        for tool in ["bash_exec", "bash_spawn"] {
            for env in [Value::Null, json!({"HTTP_PROXY":"caller", "KEEP":"yes"})] {
                let (output, calls) = Box::pin(run(
                    package,
                    tool,
                    json!({"env":{"HTTP_PROXY":"default", "SSL_CERT_FILE":"ca"}, "filesystem":{}}),
                    env.clone(),
                ))
                .await;
                assert_eq!(output["command"], "unchanged");
                assert_eq!(output["env"]["SSL_CERT_FILE"], "ca");
                assert_eq!(
                    output["env"]["HTTP_PROXY"],
                    if env.is_null() { "default" } else { "caller" }
                );
                assert_eq!(calls, ["proxy_auth_environment", tool]);
            }
        }
    }
}

#[tokio::test]
async fn middleware_proxy_policy_helper_failure_never_dispatches_next() {
    for reply in [
        json!({"is_error":true,"error":"unavailable"}),
        json!({"isError":true}),
        json!({"env":null}),
        json!({}),
    ] {
        for tool in ["bash_exec", "bash_spawn"] {
            let (output, calls) = Box::pin(run("coding", tool, reply.clone(), Value::Null)).await;
            assert_eq!(output["is_error"], true, "{output}");
            assert_eq!(calls, ["proxy_auth_environment"]);
        }
    }
}
